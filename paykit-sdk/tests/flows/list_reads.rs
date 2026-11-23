//! How often a list read binds a record: once per record it lists. Each bind checks every
//! stored message the record holds (Paykit wire 5.3), so a list that bound a record again
//! for its periods, its kept stops or its pause would cost a multiple of that.

use std::sync::Arc;

use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig, ReceiptsOf, RequestStatus,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;

use crate::common::{draft, request_from, store, World};

/// Alice's wallet on `store` with five requests from bob's shop, three paid and receipted
/// and two open. Its records hold two certificates: the shop's, on its requests and
/// receipts, and the wallet's own, on its acceptances and proofs.
async fn five_requests(w: &World, store: Arc<dyn StateStore>) -> (Paykit, Paykit) {
    let mut wallet = w.wallet_on("wallet", store).await;
    let mut shop = w.shop().await;
    let mut requests = Vec::new();
    for value in ["1", "2", "3", "4", "5"] {
        requests.push(request_from(&mut shop, w.alice_id(), draft(value, w.clock.now_ms())).await);
    }
    wallet.receive(None).await.unwrap();
    // Three are paid and receipted; two stay open.
    for request in &requests[..3] {
        let paid = wallet
            .pay_request(request, PayOptions::default())
            .await
            .outcome
            .unwrap();
        assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    }
    shop.receive(None).await.unwrap();
    for sale in shop.payee_activity().await.unwrap() {
        if sale.attempt_id.is_some() {
            assert!(shop.confirm_payment(&sale.record).await.unwrap().is_some());
        }
    }
    wallet.receive(None).await.unwrap();
    (wallet, shop)
}

#[tokio::test]
async fn test_list_reads_bind_each_record_once() {
    let w = World::new();
    let (wallet, shop) = Box::pin(five_requests(&w, store())).await;

    let before = wallet.activity_binds();
    let shown = wallet.requests().await.unwrap();
    assert_eq!(
        wallet.activity_binds() - before,
        5,
        "one bind per payer record"
    );
    let paid = shown
        .iter()
        .filter(|view| view.status == RequestStatus::Paid)
        .count();
    assert_eq!((shown.len(), paid), (5, 3), "{shown:?}");

    let before = wallet.activity_binds();
    assert_eq!(wallet.receipts(ReceiptsOf::All).await.unwrap().len(), 3);
    assert_eq!(wallet.activity_binds() - before, 5);

    let before = shop.activity_binds();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 5);
    assert_eq!(
        shop.activity_binds() - before,
        5,
        "one bind per payee record"
    );
}

/// Alice's wallet loaded again from `store`, as at an app's next start.
async fn reloaded(w: &World, store: Arc<dyn StateStore>, app_id: [u8; 16]) -> Paykit {
    w.load_on_clock(&w.alice, store, app_id, Arc::clone(&w.clock))
        .await
}

#[tokio::test]
async fn test_list_reads_verify_each_certificate_once_per_handle() {
    // A fresh handle verifies each of the two certificates its records hold once, at its
    // first read, and none on a repeat read; the handle before it verified them in its own.
    let w = World::new();
    let shared = store();
    let (wallet, _shop) = Box::pin(five_requests(&w, Arc::clone(&shared))).await;
    let app_id = wallet.app_id();
    drop(wallet);
    let wallet = Box::pin(reloaded(&w, Arc::clone(&shared), app_id)).await;
    assert_eq!(wallet.certificate_verifications(), 0);
    assert_eq!(wallet.requests().await.unwrap().len(), 5);
    assert_eq!(wallet.certificate_verifications(), 2);
    assert_eq!(wallet.requests().await.unwrap().len(), 5);
    assert_eq!(wallet.receipts(ReceiptsOf::All).await.unwrap().len(), 3);
    assert_eq!(wallet.payments().await.unwrap().len(), 3);
    assert_eq!(
        wallet.certificate_verifications(),
        2,
        "none on a repeat read"
    );

    // Loaded again, the next handle verifies them again.
    drop(wallet);
    let wallet = Box::pin(reloaded(&w, shared, app_id)).await;
    assert_eq!(wallet.requests().await.unwrap().len(), 5);
    assert_eq!(wallet.certificate_verifications(), 2);
}

#[tokio::test]
async fn test_list_reads_restore_verifies_every_certificate_again() {
    let w = World::new();
    let (mut wallet, _shop) = Box::pin(five_requests(&w, store())).await;
    wallet.requests().await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(wallet.certificate_verifications(), 0);
    Box::pin(crate::common::finish_listing(&mut wallet)).await;
    assert_eq!(wallet.requests().await.unwrap().len(), 5);
    assert_eq!(wallet.certificate_verifications(), 2);
}

#[tokio::test]
async fn test_list_reads_renewed_certificate_clears_the_verified_ones() {
    // The wallet takes a renewal of its own certificate: what it verified under the one
    // before is verified again.
    let w = World::new();
    let (mut wallet, _shop) = Box::pin(five_requests(&w, store())).await;
    wallet.requests().await.unwrap();
    let before = wallet.certificate_verifications();
    let old = wallet.app().cert().clone();
    let me = wallet.app_id();
    let renewed =
        crate::certificate::renew(&w, me, &AppScopes::new().pay().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    crate::certificate::list(&w, me, &old, &renewed);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), renewed.hash());
    assert_eq!(wallet.requests().await.unwrap().len(), 5);
    assert_eq!(wallet.certificate_verifications() - before, 2);
}
