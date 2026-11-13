//! One-time requests (Paykit spec 8.1): sent, received, accepted, paid, proven and
//! receipted; rejected and cancelled from each side.

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{MessageDelivery, PayOptions, PayOutcome, PayeeStatus, Refusal, RequestStatus};
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
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
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
async fn test_one_time_rail_refusal_writes_failed_and_may_be_paid_again() {
    // A refusal says nothing was and nothing will be submitted (Paykit spec 7.2, "Failed
    // means definitively not paid"): the attempt is written `failed` and the journal entry
    // finished, so a resume repeats nothing and a replacement payment goes ahead.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    w.rail.next(Behaviour::Refuse);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Failed { .. }), "{outcome:?}");
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = again else {
        panic!("{again:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
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
        matches!(refused.refusal(), Some(Refusal::Transfer { .. })),
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
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
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
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

// ---------------------------------------------------------------------------------------
// The kept-copy stop (Paykit spec 8.1): a payee's cancellation that can't be placed in its
// record still refuses every new attempt

/// The next `n` batches the wallet's switch is sent are refused, `403`: the app's
/// placements of a kept cancellation among them.
fn refuse_batches(w: &World, n: usize) {
    for _ in 0..n {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
}

/// Alice's wallet on `store`, holding bob's request, which bob then cancels; the wallet
/// receives the cancellation, and its placement in the record is refused.
async fn cancelled_unplaced(
    w: &World,
    store: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) -> (paykit_sdk::Paykit, paykit_sdk::RequestRef) {
    let mut wallet = w.wallet_on("wallet", store).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    refuse_batches(w, 1);
    wallet.receive(None).await.unwrap();
    (wallet, request)
}

/// The record holds no cancellation: the placement failed.
async fn record_uncancelled(wallet: &paykit_sdk::Paykit, request: &paykit_sdk::RequestRef) {
    let key = wallet.request(request).await.unwrap().record;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(
        value["cancellation"].is_null(),
        "the cancellation was placed"
    );
}

#[tokio::test]
async fn test_one_time_kept_stop_refuses_a_first_payment_it_couldnt_place() {
    let w = World::new();
    let (mut wallet, request) = cancelled_unplaced(&w, crate::common::store()).await;
    record_uncancelled(&wallet, &request).await;
    // The placement before the payment is refused too: the kept stop refuses it.
    refuse_batches(&w, 1);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_kept_stop_refuses_a_payment_after_a_resume() {
    let w = World::new();
    let store = crate::common::store();
    let (wallet, request) = cancelled_unplaced(&w, std::sync::Arc::clone(&store)).await;
    let app_id = wallet.app_id();
    drop(wallet);
    // Restarted: the resume's placement and the one before the payment are refused.
    let mut wallet = w
        .load_on_clock(&w.alice, store, app_id, std::sync::Arc::clone(&w.clock))
        .await;
    refuse_batches(&w, 1);
    let _ = wallet.resume_payments().await;
    record_uncancelled(&wallet, &request).await;
    refuse_batches(&w, 1);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_kept_stop_refuses_a_payment_after_a_restore() {
    let w = World::new();
    let (mut wallet, request) = cancelled_unplaced(&w, crate::common::store()).await;
    // A backup holding the kept stop, restored; its placements are refused.
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    refuse_batches(&w, 1);
    let _ = app.reconcile().await;
    app.end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    record_uncancelled(&app, &request).await;
    refuse_batches(&w, 1);
    let refused = app
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_request_waits_for_the_payer_to_admit_the_payee() {
    // Alice's wallet hasn't invited bob: her switch doesn't admit his request, which waits
    // for her, not for the shop's certificate. Once she invites him, it goes.
    let w = World::new();
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    let mut shop = w.shop().await;
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let since = w.clock.now_ms();
    assert!(
        matches!(
            sent.delivery,
            MessageDelivery::WaitingForAdmission { since_ms, tries: 1 } if since_ms == since
        ),
        "{:?}",
        sent.delivery
    );
    assert_eq!(
        shop.message_delivery(&sent.msg_id).await.unwrap(),
        Some(sent.delivery.clone())
    );
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&sent.msg_id).await.unwrap(),
            Some(MessageDelivery::Accepted { .. })
        ),
        "{:?}",
        shop.message_delivery(&sent.msg_id).await
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_one_time_request_kept_is_sent_once_by_the_next_write() {
    // The request's batch can't connect: the client keeps it. The call reports the request
    // sent and kept, not a failure a host would answer with a second request; the next
    // flush sends the kept batch, and alice gets one request.
    let w = World::new();
    w.alice.switch().keep_passed_entries();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    assert!(
        matches!(sent.delivery, MessageDelivery::Kept { .. }),
        "{:?}",
        sent.delivery
    );
    assert_eq!(
        shop.message_delivery(&sent.msg_id).await.unwrap(),
        Some(sent.delivery.clone())
    );
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&sent.msg_id).await.unwrap(),
            Some(MessageDelivery::Accepted { .. })
        ),
        "{:?}",
        shop.message_delivery(&sent.msg_id).await
    );
    wallet.receive(None).await.unwrap();
    let requests = wallet.requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(Some(requests[0].request), sent.request);
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), 1);
}

fn unreachable_ops() -> pubky_switch_client::testing::Fault {
    pubky_switch_client::testing::Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    }
}

#[tokio::test]
async fn test_one_time_attempt_kept_unsent_stays_recorded_through_a_resume() {
    // The attempt's batch can't connect and is kept. A resume whose flush still can't send
    // it leaves the entry as it was: the claim can't show the attempt yet. Once the batch
    // goes, the resume finds the attempt open with no payment started and writes `failed`,
    // and the user pays once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.alice.switch().fail_next(unreachable_ops());
    let kept = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(kept.kept_batch().is_some(), "{kept:?}");
    let stage = |payments: Vec<paykit_sdk::PaymentView>| payments[0].stage;
    assert_eq!(
        stage(wallet.payments().await.unwrap()),
        paykit_sdk::Stage::Recorded
    );
    w.alice.switch().fail_next(unreachable_ops());
    w.alice.switch().fail_next(unreachable_ops());
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        stage(wallet.payments().await.unwrap()),
        paykit_sdk::Stage::Recorded
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
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_one_time_request_kept_then_refused_reads_as_failed() {
    // The request's batch is kept, then refused for good when it is sent again: the
    // request never went out, and reads as failed, not as one delivered and forgotten.
    let w = World::new();
    let mut shop = w.shop().await;
    w.bob.switch().fail_next(unreachable_ops());
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let MessageDelivery::Kept { batch_id } = sent.delivery else {
        panic!("{:?}", sent.delivery);
    };
    assert_eq!(
        shop.message_delivery(&sent.msg_id).await.unwrap(),
        Some(MessageDelivery::Kept { batch_id })
    );
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&sent.msg_id).await.unwrap(),
            Some(MessageDelivery::Failed { .. })
        ),
        "{:?}",
        shop.message_delivery(&sent.msg_id).await
    );
    assert!(shop.payee_activity().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_one_time_request_kept_then_refused_at_a_read_reads_as_failed_once_acknowledged() {
    // The read sends the kept batch again and the switch refuses it: the receive takes the
    // client's event from its report, so the request reads as failed even after the host
    // acknowledged that event.
    let w = World::new();
    let mut shop = w.shop().await;
    w.bob.switch().fail_next(unreachable_ops());
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let summary = shop.receive(None).await.unwrap();
    let refused = summary
        .report
        .events
        .iter()
        .find_map(|event| match event {
            pubky_switch_client::app::AppEvent::BatchRefused { id, .. } => Some(*id),
            _ => None,
        })
        .expect("the refusal is reported");
    shop.app_mut().ack_events(refused).await.unwrap();
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&sent.msg_id).await.unwrap(),
            Some(MessageDelivery::Failed { .. })
        ),
        "{:?}",
        shop.message_delivery(&sent.msg_id).await
    );
}

/// A request bob's shop sends to alice while his switch can't be reached: the client keeps
/// its batch. Returns the request's `msg_id` and the kept batch's ID.
async fn kept_request(w: &World, shop: &mut paykit_sdk::Paykit) -> (String, [u8; 16]) {
    w.bob.switch().fail_next(unreachable_ops());
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let MessageDelivery::Kept { batch_id } = sent.delivery else {
        panic!("{:?}", sent.delivery);
    };
    (sent.msg_id, batch_id)
}

#[tokio::test]
async fn test_one_time_request_kept_reads_as_kept_after_a_store_failure_and_a_reload() {
    // The call's commits are the client's own: the one that keeps the request's batch,
    // indexing its message, and its send's. The store fails the first commit after them,
    // as it would fail a write of the SDK's own, and the reloaded shop still reads the
    // request kept. Its next flush sends it.
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let base = crate::common::store();
    let shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&base),
        )
        .await;
    let me = shop.app_id();
    drop(shop);
    let fault = std::sync::Arc::new(pubky_switch_client::store::FaultStore::new(
        std::sync::Arc::clone(&base),
        pubky_switch_client::store::Fault::FailBefore(3),
    ));
    let mut shop = w
        .load_on_clock(
            &w.bob,
            std::sync::Arc::clone(&fault)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
            me,
            std::sync::Arc::clone(&w.clock),
        )
        .await;
    assert_eq!(fault.commits(), 0, "a load commits nothing here");
    let (msg_id, batch_id) = kept_request(&w, &mut shop).await;
    assert_eq!(fault.commits(), 2);
    assert_eq!(
        shop.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Kept { batch_id })
    );
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, base, me, std::sync::Arc::clone(&w.clock))
        .await;
    assert_eq!(
        shop.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Kept { batch_id })
    );
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&msg_id).await.unwrap(),
            Some(MessageDelivery::Accepted { .. })
        ),
        "{:?}",
        shop.message_delivery(&msg_id).await
    );
}

#[tokio::test]
async fn test_one_time_request_kept_past_the_retention_then_refused_reads_as_failed() {
    // The request's batch stays kept longer than the sent-message retention, then is
    // refused for good: the request reads as failed from the drop's time, for the
    // retention after it, not as one delivered and forgotten.
    let w = World::new();
    let mut shop = w.shop().await;
    let (msg_id, _) = kept_request(&w, &mut shop).await;
    w.clock
        .advance_ms(pubky_switch_client::app::SENT_RETENTION_MS + HOUR_MS);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    shop.flush().await.unwrap();
    let dropped_at = w.clock.now_ms();
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&msg_id).await.unwrap(),
            Some(MessageDelivery::Failed { failed_at_ms, .. }) if failed_at_ms == dropped_at
        ),
        "{:?}",
        shop.message_delivery(&msg_id).await
    );
}

/// A request alice's wallet has received, open for 60 days.
async fn long_request(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
) -> paykit_sdk::RequestRef {
    let draft = draft("1", w.clock.now_ms())
        .with_proposal_expires_at_ms(w.clock.now_ms() + 60 * 24 * HOUR_MS);
    let request = request_from(shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    request
}

#[tokio::test]
async fn test_one_time_attempt_kept_then_forgotten_reads_the_claim_and_pays_once() {
    // The attempt's batch is kept, then refused for good, and its end forgotten past the
    // retention: the client knows no batch of the attempt. That says nothing of the claim,
    // so the resume reads it, finds no attempt and closes the entry. Nothing was paid, and
    // the user's next payment pays once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = long_request(&w, &mut wallet, &mut shop).await;
    w.alice.switch().fail_next(unreachable_ops());
    let kept = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    let batch_id = kept.kept_batch().expect("the attempt's batch is kept");
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    wallet.flush().await.unwrap();
    assert!(
        matches!(
            wallet.app().kept_batch(batch_id).await.unwrap(),
            Some(pubky_switch_client::app::KeptBatchStatus::Dropped { .. })
        ),
        "the refusal drops the batch"
    );
    w.clock
        .advance_ms(pubky_switch_client::app::SENT_RETENTION_MS + HOUR_MS);
    wallet.flush().await.unwrap();
    assert_eq!(wallet.app().kept_batch(batch_id).await.unwrap(), None);
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Recorded
    );
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Abandoned
    );
    assert!(w.rail.all_executions().is_empty());
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_one_time_attempt_kept_at_the_backup_restored_never_pays_again() {
    // The backup is taken while the attempt's batch is kept. The wallet goes on: the batch
    // goes, its attempt is written `failed`, and the user pays. The restored app's client
    // knows no batch of the backup's attempt: it reads the claim, which shows the attempt
    // ended, and pays nothing; the request reads paid.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = long_request(&w, &mut wallet, &mut shop).await;
    w.alice.switch().fail_next(unreachable_ops());
    let kept = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(kept.kept_batch().is_some(), "{kept:?}");
    let backup = wallet.backup().await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    drop(wallet);
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
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
    restored.resume_payments().await.unwrap();
    assert!(
        restored.payments().await.unwrap().iter().all(|p| !matches!(
            p.stage,
            paykit_sdk::Stage::Recorded | paykit_sdk::Stage::Attempted | paykit_sdk::Stage::Paying
        )),
        "{:?}",
        restored.payments().await
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    let again = restored
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(again.refusal(), Some(&Refusal::AlreadyPaid), "{again:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_one_time_expired_request_not_found_at_the_switch_is_not_found() {
    // Past the request's expiry the switch answers the record's read `404`, though the
    // wallet keeps its open copy: nothing shows the request open, so it isn't refused as
    // expired, and nothing is paid.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut short = draft("1", w.clock.now_ms());
    short.proposal_expires_at_ms = Some(w.clock.now_ms() + HOUR_MS);
    let request = request_from(&mut shop, w.alice_id(), short).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/records/".into(),
            code: pubky_switch_core::http::ErrorCode::NotFound,
            current: serde_json::Value::Null,
        });
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::NotFound), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_attempt_kept_then_refused_reads_the_claim_while_its_end_is_known() {
    // The attempt's batch is kept, then refused for good: the client answers it `Dropped`
    // within the retention. A dropped batch is no batch still kept, so the resume reads the
    // claim at once, finds no attempt and closes the entry, rather than leave it waiting for
    // a batch that never goes; the user's next payment pays once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = long_request(&w, &mut wallet, &mut shop).await;
    w.alice.switch().fail_next(unreachable_ops());
    let batch_id = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err()
        .kept_batch()
        .expect("the attempt's batch is kept");
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    wallet.flush().await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            wallet.app().kept_batch(batch_id).await.unwrap(),
            Some(pubky_switch_client::app::KeptBatchStatus::Dropped { .. })
        ),
        "the client still knows the batch dropped"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Abandoned
    );
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_one_time_attempt_kept_then_applied_writes_failed_and_pays_once() {
    // The attempt's batch is kept and applied by the next flush: the attempt is open with
    // no payment started, a payment in progress until the resume reads the claim and
    // writes `failed`. The next payment pays once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = long_request(&w, &mut wallet, &mut shop).await;
    w.alice.switch().fail_next(unreachable_ops());
    let batch_id = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err()
        .kept_batch()
        .expect("the attempt's batch is kept");
    wallet.flush().await.unwrap();
    assert!(
        matches!(
            wallet.app().kept_batch(batch_id).await.unwrap(),
            Some(pubky_switch_client::app::KeptBatchStatus::Applied { .. })
        ),
        "{:?}",
        wallet.app().kept_batch(batch_id).await
    );
    let busy = wallet
        .pay_request(&request, PayOptions::default())
        .await
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
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_one_time_expired_request_read_refused_by_the_switch_keeps_its_code() {
    // Past the request's expiry the switch refuses the record's read for good: the
    // refusal is the answer, with the switch's code, never taken as the record unchanged.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut short = draft("1", w.clock.now_ms());
    short.proposal_expires_at_ms = Some(w.clock.now_ms() + HOUR_MS);
    let request = request_from(&mut shop, w.alice_id(), short).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/records/".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        Some(pubky_switch_core::http::ErrorCode::PermissionMissing),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

/// `n` requests from bob lasting 90 days, whose records the wallet holds sealed under a key
/// it doesn't hold yet: the phone wrote them again under an epoch's key the wallet never got,
/// and the wallet holds only a later one. Returns the requests and the wallet's certificate,
/// for the key bundle that opens them.
async fn sealed_requests(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    phone: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    n: usize,
) -> (
    Vec<paykit_sdk::RequestRef>,
    pubky_switch_core::records::AppCert,
) {
    let days = pubky_switch_core::records::DAY_MS;
    let mut requests = Vec::new();
    for i in 0..n {
        let mut lasting =
            draft("1", w.clock.now_ms()).with_proposal_expires_at_ms(w.clock.now_ms() + 90 * days);
        lasting.payment_reference =
            paykit_lib::PaymentReference::new(&format!("order-{}", i + 1)).unwrap();
        requests.push(request_from(shop, w.alice_id(), lasting).await);
    }
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    for request in &requests {
        let value = crate::outcome_record::record_value(phone, request)
            .await
            .unwrap();
        crate::outcome_record::phone_writes(
            phone,
            request,
            Some(&serde_json::to_vec(&value).unwrap()),
        )
        .await;
    }
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(w, wallet.app_id(), &cert, newest);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    for request in &requests {
        let key = wallet
            .app()
            .record_key(&crate::outcome_record::payer_record(request))
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
    }
    (requests, cert)
}

/// Bob's shop cancels `request`.
async fn shop_cancels(shop: &mut paykit_sdk::Paykit, request: &paykit_sdk::RequestRef) {
    let record = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap()
        .record;
    shop.cancel_sent_request(&record, None).await.unwrap();
}

#[tokio::test]
async fn test_one_time_cancellation_while_its_record_awaits_its_key_stops_it_past_30_days() {
    // The wallet holds the request's record sealed under a key it doesn't hold yet. Bob
    // cancels. The stop is kept at once, the entry waiting for no key, and 30 days on no
    // give-up or bound takes it: once the key comes, the request is cancelled and never
    // payable (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (requests, cert) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let request = requests[0];
    shop_cancels(&mut shop, &request).await;
    let got = wallet.receive(None).await.unwrap();
    assert!(got.key_epochs_awaited.is_empty(), "{got:?}");
    // Past 30 days from its acceptance, nothing is given up: the stop stands.
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet.notices().await.unwrap().is_empty());
    // The key comes: the record opens, and the stop goes into it.
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
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
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["cancellation"].is_null(), "{value}");
}

/// Whether `wallet` keeps a stop for bob's request `hash`.
async fn keeps_stop(w: &World, wallet: &paykit_sdk::Paykit, hash: [u8; 32]) -> bool {
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    let key = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
    let stop = format!("kc/{}", pubky_switch_core::enc::b64(&key));
    wallet
        .app()
        .protocol_scan(stop.as_bytes())
        .await
        .unwrap()
        .iter()
        .any(|(found, _)| found == stop.as_bytes())
}

#[tokio::test]
async fn test_one_time_stops_for_requests_not_held_bounded_per_sender() {
    // Bob cancels a request the wallet holds sealed, then 64 it holds no record of, a
    // millisecond apart, then a second one it holds sealed: all are kept, the stops for the
    // requests held counting in no bound. A 65th cancellation of a request not held drops
    // the earliest of those, never a stop for a request held (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (held, _) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 2).await;
    shop_cancels(&mut shop, &held[0]).await;
    wallet.receive(None).await.unwrap();
    let unknown = |i: usize| [u8::try_from(i + 1).unwrap(); 32];
    for i in 0..paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        w.clock.advance_ms(1);
        cancel_to(&w, &[wallet.app_id()], unknown(i));
    }
    wallet.receive(None).await.unwrap();
    shop_cancels(&mut shop, &held[1]).await;
    wallet.receive(None).await.unwrap();
    let all = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER + 2;
    assert_eq!(wallet.app().protocol_scan(b"kc/").await.unwrap().len(), all);
    w.clock.advance_ms(1);
    cancel_to(
        &w,
        &[wallet.app_id()],
        unknown(paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER),
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"kc/").await.unwrap().len(), all);
    for request in &held {
        assert!(keeps_stop(&w, &wallet, request.request_hash).await);
    }
    assert!(!keeps_stop(&w, &wallet, unknown(0)).await);
    assert!(keeps_stop(&w, &wallet, unknown(1)).await);
    assert!(
        keeps_stop(
            &w,
            &wallet,
            unknown(paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER)
        )
        .await
    );
}

#[tokio::test]
async fn test_one_time_stop_for_a_request_not_held_goes_30_days_after_its_acceptance() {
    // Bob cancels a request the wallet holds no record of: the stop is kept a day short of 30
    // days from its acceptance, and goes at 30 days, on the service time verified.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let hash = [7; 32];
    cancel_to(&w, &[wallet.app_id()], hash);
    wallet.receive(None).await.unwrap();
    assert!(keeps_stop(&w, &wallet, hash).await);
    let day = pubky_switch_core::records::DAY_MS;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - day);
    wallet.receive(None).await.unwrap();
    assert!(keeps_stop(&w, &wallet, hash).await);
    w.clock.advance_ms(day);
    wallet.receive(None).await.unwrap();
    assert!(!keeps_stop(&w, &wallet, hash).await);
}

/// Send the wallet's own housekeeping now, its next receive key's registration and a
/// deferred cursor move (Switch spec 3.6, 8.2), so that its next receive sends the switch
/// only Paykit's writes, which the switch is set to refuse.
async fn housekeeping(wallet: &mut paykit_sdk::Paykit) {
    wallet.app_mut().rotate().await.unwrap();
    wallet.app_mut().flush_cursor().await.unwrap();
}

/// Bob's cancellation of a request the wallet holds no record of yet, then, 20 days on, the
/// request itself, whose write the switch refuses: the wallet keeps a stop for it and the
/// request to write later. Returns the request.
async fn stop_then_request_stored(w: &World, wallet: &mut paykit_sdk::Paykit) -> [u8; 32] {
    let (request, hash) = unsent_request(w);
    cancel_to(w, &[wallet.app_id()], hash);
    wallet.receive(None).await.unwrap();
    assert!(keeps_stop(w, wallet, hash).await);
    w.clock.advance_ms(20 * pubky_switch_core::records::DAY_MS);
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    // The receive step's write, and the app's first write of it, are refused.
    housekeeping(wallet).await;
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.app().protocol_scan(b"se/").await.unwrap().len(),
        1,
        "the request is kept to write later"
    );
    hash
}

#[tokio::test]
async fn test_one_time_stop_for_a_request_kept_to_write_stays_past_30_days() {
    // 30 days after the cancellation's acceptance, the request still waits to be written:
    // the stop stays, and once room frees the request is written cancelled, never payable
    // (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let hash = stop_then_request_stored(&w, &mut wallet).await;
    let day = pubky_switch_core::records::DAY_MS;
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS - 20 * day + day);
    housekeeping(&mut wallet).await;
    crate::common::refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    assert!(keeps_stop(&w, &wallet, hash).await);
    // Room frees: the request is written, the stop placed in its record.
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"se/").await.unwrap().is_empty());
    assert!(!keeps_stop(&w, &wallet, hash).await);
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_stop_for_a_request_given_up_goes_past_30_days() {
    // The request arrives a day after its cancellation and is never written: 30 days after
    // its acceptance it is given up, and the stop, no longer kept for a message to write,
    // goes with it, 30 days having passed since its own acceptance.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let (request, hash) = unsent_request(&w);
    cancel_to(&w, &[wallet.app_id()], hash);
    wallet.receive(None).await.unwrap();
    let day = pubky_switch_core::records::DAY_MS;
    w.clock.advance_ms(day);
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    housekeeping(&mut wallet).await;
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    // A day short of 30 days since the request's acceptance, both stay.
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - day);
    housekeeping(&mut wallet).await;
    crate::common::refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    assert!(keeps_stop(&w, &wallet, hash).await);
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
    // At 30 days the request is given up, and the stop goes.
    w.clock.advance_ms(day);
    housekeeping(&mut wallet).await;
    crate::common::refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"se/").await.unwrap().is_empty());
    assert!(!keeps_stop(&w, &wallet, hash).await);
}

#[tokio::test]
async fn test_one_time_stops_bound_never_drops_a_stop_for_a_request_kept_to_write() {
    // Bob's stop for a request kept to write is his earliest. 65 cancellations of requests
    // the wallet holds no record of follow: the bound drops the earliest of those, never
    // the stop kept for the request, which counts in no bound (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let hash = stop_then_request_stored(&w, &mut wallet).await;
    let unknown = |i: usize| [u8::try_from(i + 1).unwrap(); 32];
    let max = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..max {
        w.clock.advance_ms(1);
        cancel_to(&w, &[wallet.app_id()], unknown(i));
    }
    housekeeping(&mut wallet).await;
    crate::common::refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.app().protocol_scan(b"kc/").await.unwrap().len(),
        max + 1
    );
    w.clock.advance_ms(1);
    cancel_to(&w, &[wallet.app_id()], unknown(max));
    housekeeping(&mut wallet).await;
    crate::common::refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.app().protocol_scan(b"kc/").await.unwrap().len(),
        max + 1
    );
    assert!(keeps_stop(&w, &wallet, hash).await);
    assert!(!keeps_stop(&w, &wallet, unknown(0)).await);
    assert!(keeps_stop(&w, &wallet, unknown(max)).await);
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_one_time_cancellation_of_a_request_not_held_kept_as_stop_and_reply_at_an_app_paying_and_requesting(
) {
    // Alice's till both pays and requests. Bob cancels a request it holds on neither side:
    // it keeps the cancellation as the stop and as a payer's reply, one of each.
    let w = World::new();
    let till = w
        .join(
            &w.alice,
            "till",
            paykit_sdk::AppScopes::new()
                .pay()
                .request()
                .build()
                .unwrap(),
        )
        .await;
    let mut till = till;
    till.app_mut().invite(&w.bob_id()).await.unwrap();
    let _shop = w.shop().await;
    let hash = [7; 32];
    cancel_to(&w, &[till.app_id()], hash);
    till.receive(None).await.unwrap();
    assert!(keeps_stop(&w, &till, hash).await);
    assert_eq!(till.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    assert_eq!(till.app().protocol_scan(b"kc/").await.unwrap().len(), 1);
}

/// `app` pays `request`: the acceptance goes with the dispatch, then the method reports the
/// payment failed, so the claim is free again and the record keeps the acceptance.
async fn failed_after_acceptance(
    w: &World,
    app: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) {
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = app
        .pay_request(request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    w.rail.settle(&attempt_id, false);
    app.resume_payments().await.unwrap();
}

/// Whether `refused` is the refusal of a rejection of a request accepted.
fn accepted_refusal(refused: &paykit_sdk::PaykitError) -> bool {
    matches!(refused.refusal(), Some(Refusal::Usage(text)) if text == "accepted; cancel instead")
}

#[tokio::test]
async fn test_one_time_rejection_after_a_failed_payments_acceptance_refused_and_cancelled() {
    // The wallet's payment failed after its acceptance went: the request is open again, but
    // after an acceptance the payer may only cancel (Paykit spec 8.1, "Rejecting and
    // cancelling"), so a rejection is refused and the cancellation goes.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    failed_after_acceptance(&w, &mut wallet, &request).await;
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    assert!(accepted_refusal(&refused), "{refused:?}");
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Accepted
    );
    wallet.cancel_request(&request, None).await.unwrap();
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Cancelled
    );
}

#[tokio::test]
async fn test_one_time_rejection_after_another_own_apps_acceptance_refused() {
    // The phone's acceptance went; the wallet holds the record with it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    phone.receive(None).await.unwrap();
    failed_after_acceptance(&w, &mut phone, &request).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    assert!(accepted_refusal(&refused), "{refused:?}");
    // Cancelling is what the payer does instead.
    wallet.cancel_request(&request, None).await.unwrap();
}

#[tokio::test]
async fn test_one_time_rejection_of_a_request_not_accepted_sent() {
    // A request with no acceptance is rejected as before; then no payment accepts it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Rejected));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_rejection_while_the_records_are_not_listed_refused_as_not_now() {
    // After a `missed` item whose listing fails, the copy may lack the acceptance another
    // app recorded: no response is decided from it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    let unlisted = || pubky_switch_client::testing::Fault::Error {
        path: "records?collection".into(),
        code: pubky_switch_core::http::ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    };
    w.alice.switch().fail_next(unlisted());
    assert!(wallet
        .receive(None)
        .await
        .unwrap()
        .records_unlisted
        .is_some());
    w.alice.switch().fail_next(unlisted());
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_one_time_missed_item_asks_wait_for_a_sealed_records_key() {
    // The wallet's only record of bob's open request is sealed under a key it doesn't hold
    // yet: after a `missed` item it can't tell bob is a counterparty to ask, so the asks and
    // their mark wait; once the key comes, bob is asked (Paykit spec 9).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, cert) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(wallet.app().protocol_scan(b"mi/").await.unwrap().len(), 1);
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(crate::common::asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(wallet.app().protocol_scan(b"mi/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_one_time_restore_asks_wait_for_a_sealed_records_key() {
    // The wallet's only record of bob's open request is sealed under a key it doesn't hold
    // yet. Restored, its reconcile can't tell bob is a counterparty to ask: the restore's
    // asks wait, and go once the key comes (Paykit spec 7.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, cert) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let backup = wallet.backup().await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    assert!(crate::common::asked(&restored, &w.bob_id())
        .await
        .is_empty());
    crate::unrequested::store_bundle(&w, app_id, &cert);
    for _ in 0..3 {
        restored.receive(None).await.unwrap();
    }
    assert_eq!(crate::common::asked(&restored, &w.bob_id()).await.len(), 1);
}

/// The value Paykit keeps at `key` in `app`'s state.
async fn state_value(app: &paykit_sdk::Paykit, key: &[u8]) -> Vec<u8> {
    app.app()
        .protocol_scan(key)
        .await
        .unwrap()
        .into_iter()
        .find(|(found, _)| found == key)
        .unwrap()
        .1
}

/// The stop key of bob's request `hash` at `wallet`.
fn stop_key(w: &World, wallet: &paykit_sdk::Paykit, hash: [u8; 32]) -> Vec<u8> {
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    let key = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
    format!("kc/{}", pubky_switch_core::enc::b64(&key)).into_bytes()
}

#[tokio::test]
async fn test_one_time_stop_naming_another_request_refused_as_corrupt() {
    // The stop kept for one request is found at another request's key: a corrupt store. The
    // receive fails rather than place it in the other request's record, and the request
    // isn't shown cancelled by it (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let other = [7; 32];
    cancel_to(&w, &[wallet.app_id()], other);
    wallet.receive(None).await.unwrap();
    let value = state_value(&wallet, &stop_key(&w, &wallet, other)).await;
    let key = stop_key(&w, &wallet, request.request_hash);
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put { key, value }])
        .await
        .unwrap();
    let failed = wallet.receive(None).await.unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    let failed = wallet.request(&request).await.unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(value["cancellation"].is_null(), "{value}");
}

#[tokio::test]
async fn test_one_time_stop_at_its_own_key_read_back() {
    // The same stop at its own key reads back: the request it names shows cancelled.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let (request, hash) = unsent_request(&w);
    cancel_to(&w, &[wallet.app_id()], hash);
    wallet.receive(None).await.unwrap();
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    wallet.receive(None).await.unwrap();
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_one_time_no_response_after_the_payers_own_cancellation() {
    // After the payer's cancellation, nothing more goes for the request: no rejection, no
    // second cancellation, no payment (Paykit spec 8.1, one response per request).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.cancel_request(&request, None).await.unwrap();
    for refused in [
        wallet.reject_request(&request, None).await.unwrap_err(),
        wallet.cancel_request(&request, None).await.unwrap_err(),
        wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err(),
    ] {
        assert_eq!(refused.refusal(), Some(&Refusal::Cancelled), "{refused:?}");
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_no_response_after_the_payees_cancellation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    shop_cancels(&mut shop, &request).await;
    wallet.receive(None).await.unwrap();
    for refused in [
        wallet.reject_request(&request, None).await.unwrap_err(),
        wallet.cancel_request(&request, None).await.unwrap_err(),
    ] {
        assert_eq!(refused.refusal(), Some(&Refusal::Cancelled), "{refused:?}");
    }
}

#[tokio::test]
async fn test_one_time_no_cancellation_after_a_rejection() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    let refused = wallet.cancel_request(&request, None).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Rejected), "{refused:?}");
}

#[tokio::test]
async fn test_one_time_no_response_over_a_record_sealed_at_a_key_not_held() {
    // The wallet holds the request's record sealed under a key it doesn't hold yet: it may
    // hold a response or a cancellation, so no response and no payment is decided from it,
    // refused as not now rather than as no request.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (requests, _) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let request = requests[0];
    for refused in [
        wallet.reject_request(&request, None).await.unwrap_err(),
        wallet.cancel_request(&request, None).await.unwrap_err(),
        wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err(),
    ] {
        assert!(
            matches!(refused.refusal(), Some(Refusal::NotNow(_))),
            "{refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_payers_cancellation_before_its_record_placed_at_an_app_paying_and_requesting(
) {
    // Bob's dual app holds `pay` and `request`. Bob's till sends alice a request and writes
    // its record; alice cancels it. The dual app reads the record and alice's cancellation
    // in one read: the payee's record shows alice's cancellation (Paykit spec 8.1, "Replies
    // before their record"), the till never reading it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut dual = w
        .join(
            &w.bob,
            "dual",
            paykit_sdk::AppScopes::new()
                .pay()
                .request()
                .build()
                .unwrap(),
        )
        .await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.cancel_request(&request, None).await.unwrap();
    wallet.flush().await.unwrap();
    dual.receive(None).await.unwrap();
    dual.receive(None).await.unwrap();
    let sales = dual.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::Cancelled, "{sales:?}");
    assert!(dual.app().protocol_scan(b"pr/").await.unwrap().is_empty());
}

/// Carol's shop, with a request to alice `wallet` takes at its next receive, each side
/// admitting the other.
async fn carols_request(w: &World, wallet: &mut paykit_sdk::Paykit) -> paykit_sdk::Paykit {
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut carol = w
        .join(
            &w.carol,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    carol.app_mut().invite(&w.alice_id()).await.unwrap();
    request_from(&mut carol, w.alice_id(), draft("2", w.clock.now_ms())).await;
    carol
}

#[tokio::test]
async fn test_one_time_missed_item_asks_the_readable_now_and_the_sealed_once_open() {
    // Carol's open request reads; bob's is sealed under a key the wallet doesn't hold yet.
    // A `missed` item asks carol at once, and keeps its mark for the sealed record; once
    // the key comes, bob is asked, and carol isn't asked again (Paykit spec 9).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let _carol = carols_request(&w, &mut wallet).await;
    let (_, cert) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(crate::common::asked(&wallet, &w.carol_id()).await.len(), 1);
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(wallet.app().protocol_scan(b"mi/").await.unwrap().len(), 1);
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(crate::common::asked(&wallet, &w.bob_id()).await.len(), 1);
    assert_eq!(crate::common::asked(&wallet, &w.carol_id()).await.len(), 1);
    assert!(wallet.app().protocol_scan(b"mi/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_one_time_restore_asks_the_readable_now_and_the_sealed_once_open() {
    // As after a `missed` item: restored, the wallet asks carol at once, and bob, whose
    // record is sealed, once the key comes; carol once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let _carol = carols_request(&w, &mut wallet).await;
    let (_, cert) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let backup = wallet.backup().await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        crate::common::asked(&restored, &w.carol_id()).await.len(),
        1
    );
    assert!(crate::common::asked(&restored, &w.bob_id())
        .await
        .is_empty());
    crate::unrequested::store_bundle(&w, app_id, &cert);
    for _ in 0..3 {
        restored.receive(None).await.unwrap();
    }
    assert_eq!(crate::common::asked(&restored, &w.bob_id()).await.len(), 1);
    assert_eq!(
        crate::common::asked(&restored, &w.carol_id()).await.len(),
        1
    );
}

#[tokio::test]
async fn test_one_time_confirming_a_request_over_a_sealed_record_refused_as_not_now() {
    // The user's confirmation that a request may be cancelled is refused as not now over a
    // record sealed under a key the wallet doesn't hold yet, as a response is: the record
    // may show a stop, so it isn't refused as no request either.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (requests, _) = sealed_requests(&w, &mut wallet, &mut phone, &mut shop, 1).await;
    let refused = wallet
        .confirm_possibly_cancelled(&requests[0], pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_one_time_cancellation_of_a_request_missing_from_a_copy_not_listed_held_until_listed()
{
    // After a `missed` item whose listing fails, bob cancels a request the copy holds no
    // record of: its absence decides nothing, so the entry is held, no stop kept, until
    // Paykit lists; then the stop is kept (Paykit spec 8.1, "Records not listed yet").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    let hash = [7; 32];
    let cancellation = cancel_to(&w, &[wallet.app_id()], hash);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "records?collection".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert!(!got.applied.contains(&cancellation), "{got:?}");
    assert!(!keeps_stop(&w, &wallet, hash).await);
    let mut applied = Vec::new();
    for _ in 0..2 {
        applied.extend(wallet.receive(None).await.unwrap().applied);
    }
    assert!(applied.contains(&cancellation), "{applied:?}");
    assert!(keeps_stop(&w, &wallet, hash).await);
}

#[tokio::test]
async fn test_one_time_stop_at_another_key_refuses_the_receive_steps_bounds() {
    // A stop is found at another request's key when a new cancellation, of a request the
    // wallet holds no record of, is taken: the receive step reads the stops for their bounds,
    // bound to their keys, and refuses the misplaced one as corrupt, keeping nothing new.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    cancel_to(&w, &[wallet.app_id()], [7; 32]);
    wallet.receive(None).await.unwrap();
    let value = state_value(&wallet, &stop_key(&w, &wallet, [7; 32])).await;
    let key = stop_key(&w, &wallet, [8; 32]);
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put { key, value }])
        .await
        .unwrap();
    cancel_to(&w, &[wallet.app_id()], [9; 32]);
    let _ = wallet.receive(None).await;
    assert!(!keeps_stop(&w, &wallet, [9; 32]).await);
}

/// `wallet` backed up, dropped and restored from that backup, reconciled.
async fn wallet_restored(w: &World, mut wallet: paykit_sdk::Paykit) -> paykit_sdk::Paykit {
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    wallet
}

#[tokio::test]
async fn test_one_time_stop_gone_at_30_days_stays_gone_after_a_restore() {
    // A stop for a request the wallet holds no record of goes at 30 days. A restore from a
    // backup taken then keeps none: the receive and the listing after it bring nothing back.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let hash = [7; 32];
    cancel_to(&w, &[wallet.app_id()], hash);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet.receive(None).await.unwrap();
    assert!(!keeps_stop(&w, &wallet, hash).await);
    let mut wallet = wallet_restored(&w, wallet).await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert!(!keeps_stop(&w, &wallet, hash).await);
}
