//! Receipts with the payee's members, and the payer's view of the receipts it holds
//! (Paykit spec 5, Paykit wire 4.11, 5.3).

use paykit_lib::periods::PeriodRef;
use paykit_lib::PaymentReference;
use paykit_sdk::{
    PayOptions, PayOutcome, PayeeStatus, ReceiptDetails, ReceiptsOf, RequestRef, UnrequestedPayment,
};
use pubky_switch_client::clock::Clock;
use serde_json::json;

use crate::common::{btc, draft, endpoint, request_from, World};

fn metadata() -> serde_json::Map<String, serde_json::Value> {
    json!({ "order": "o-7", "note": "thanks" })
        .as_object()
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn test_receipts_carry_metadata_and_are_listed_by_payee_request_and_period() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let attempt_id = outcome.attempt_id();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let msg_id = shop
        .confirm_payment_with(&record, ReceiptDetails::new().with_metadata(metadata()))
        .await
        .unwrap()
        .unwrap();
    wallet.receive(None).await.unwrap();
    let receipt = wallet.request(&request).await.unwrap().receipt.unwrap();
    assert_eq!(receipt.msg_id, msg_id);
    assert_eq!(receipt.attempt_id, attempt_id);
    assert_eq!(receipt.payee, w.bob_id());
    assert_eq!(receipt.request_hash, Some(request.request_hash));
    assert_eq!(receipt.metadata, Some(metadata()));
    // The request's reference.
    assert_eq!(receipt.payment_reference.as_deref(), Some("order-1"));
    assert_eq!(
        wallet.payments().await.unwrap()[0].receipt,
        Some(receipt.clone())
    );
    let other = RequestRef {
        payee: w.bob_id(),
        request_hash: [9; 32],
    };
    let period = PeriodRef {
        starts_at: 1,
        ends_at: 2,
    };
    for (of, listed) in [
        (ReceiptsOf::All, 1),
        (ReceiptsOf::Payee(w.bob_id()), 1),
        (ReceiptsOf::Payee(w.alice_id()), 0),
        (ReceiptsOf::Request(request), 1),
        (ReceiptsOf::Request(other), 0),
        (ReceiptsOf::Period(request, period), 0),
        (
            ReceiptsOf::Reference(PaymentReference::new("order-1").unwrap()),
            1,
        ),
        (
            ReceiptsOf::Reference(PaymentReference::new("order-2").unwrap()),
            0,
        ),
    ] {
        let listed_now = wallet.receipts(of.clone()).await.unwrap().len();
        assert_eq!(listed_now, listed, "{of:?}");
    }
}

#[tokio::test]
async fn test_receipts_of_a_request_carry_no_payment_reference() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let details =
        ReceiptDetails::new().with_payment_reference(PaymentReference::new("inv-7").unwrap());
    assert!(shop.confirm_payment_with(&record, details).await.is_err());
    // Nothing went out.
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().receipt.is_none());
}

#[tokio::test]
async fn test_receipts_of_a_payment_without_a_request_carry_the_payees_reference() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.001")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let details =
        ReceiptDetails::new().with_payment_reference(PaymentReference::new("inv-7").unwrap());
    shop.confirm_payment_with(&record, details)
        .await
        .unwrap()
        .unwrap();
    wallet.receive(None).await.unwrap();
    let receipts = wallet.receipts(ReceiptsOf::All).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].payment_reference.as_deref(), Some("inv-7"));
    assert_eq!(receipts[0].request_hash, None);
    let by_reference = wallet
        .receipts(ReceiptsOf::Reference(
            PaymentReference::new("inv-7").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(by_reference.len(), 1);
}
