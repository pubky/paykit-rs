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
