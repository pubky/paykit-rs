//! A store that fails while a message is received (Switch spec 8.2): a valid message is
//! never consumed by a store failure. Its entry is held, and applied once the store heals.
//!
//! Each case fails one read of a held record at a time, the first, then the second, and on,
//! so one of them lands in the receive step's own read, whatever the client reads first.

use std::sync::Arc;

use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, RequestStatus,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;

use crate::common::{draft, request_from, FailingRecordReads, World};

/// How many single read failures each case tries, one per run.
const READS: u64 = 8;

/// A store whose `n`th record read fails once, then reads again until `app` has received.
async fn receive_through_a_failure(app: &mut Paykit, failing: &FailingRecordReads, n: u64) {
    failing.fail_nth(n);
    let _ = app.receive(None).await;
    failing.heal();
    app.receive(None).await.unwrap();
}

async fn failing_store() -> Arc<FailingRecordReads> {
    Arc::new(FailingRecordReads::default())
}

#[tokio::test]
async fn test_receive_faults_request_applied_once_the_store_heals() {
    for n in 1..=READS {
        let w = World::new();
        let failing = failing_store().await;
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&failing) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        receive_through_a_failure(&mut wallet, &failing, n).await;
        assert_eq!(
            wallet.request(&request).await.unwrap().status,
            RequestStatus::Open,
            "read {n}"
        );
    }
}

#[tokio::test]
async fn test_receive_faults_cancellation_applied_once_the_store_heals() {
    for n in 1..=READS {
        let w = World::new();
        let failing = failing_store().await;
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&failing) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        let record = shop.payee_activity().await.unwrap()[0].record;
        shop.cancel_sent_request(&record, None).await.unwrap();
        receive_through_a_failure(&mut wallet, &failing, n).await;
        assert_eq!(
            wallet.request(&request).await.unwrap().status,
            RequestStatus::Cancelled,
            "read {n}"
        );
    }
}

#[tokio::test]
async fn test_receive_faults_proof_applied_once_the_store_heals() {
    for n in 1..=READS {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let failing = failing_store().await;
        let mut shop = w
            .join_with(
                &w.bob,
                "shop",
                AppScopes::new().request().build().unwrap(),
                PaykitConfig::default(),
                Arc::clone(&failing) as Arc<dyn StateStore>,
            )
            .await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        let outcome = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap();
        assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
        receive_through_a_failure(&mut shop, &failing, n).await;
        assert_eq!(
            shop.payee_activity().await.unwrap()[0].status,
            PayeeStatus::ProofReceived,
            "read {n}"
        );
    }
}

#[tokio::test]
async fn test_receive_faults_receipt_applied_once_the_store_heals() {
    for n in 1..=READS {
        let w = World::new();
        let failing = failing_store().await;
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&failing) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap();
        shop.receive(None).await.unwrap();
        let record = shop.payee_activity().await.unwrap()[0].record;
        assert!(shop.confirm_payment(&record).await.unwrap().is_some());
        receive_through_a_failure(&mut wallet, &failing, n).await;
        let key = wallet.request(&request).await.unwrap().record;
        let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(held.content.value().unwrap()).unwrap();
        assert!(
            !value["receipt"].is_null(),
            "read {n}: the receipt wasn't stored"
        );
    }
}
