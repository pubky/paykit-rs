//! Restore a signed payment without a request while its method is unavailable.
use super::{joined, LedgerRail};
use crate::common::{btc, endpoint, list_version, store, World};
use paykit_lib::collections::Collection;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitBackup, Rail, UnrequestedPayment,
};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
use pubky_switch_client::store::StateStore;
use std::sync::{Arc, Mutex};

struct Payment {
    world: World,
    shop: Paykit,
    wallet: Paykit,
    device: Arc<dyn StateStore>,
    rail: Arc<LedgerRail>,
    earlier: PaykitBackup,
    target: RecordTarget,
    key: [u8; 32],
    receipt: String,
    signed: Vec<u8>,
    proof_only: Vec<u8>,
}

fn target(natural: &[u8]) -> RecordTarget {
    RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        natural,
    )
}

async fn pay(wallet: &mut Paykit, world: &World) {
    let result = wallet
        .pay_without_request(
            UnrequestedPayment::new(world.bob_id(), btc("0.0015")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(result, PayOutcome::Paid { .. }));
}

async fn issue() -> Payment {
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: w.rail.clone(),
        last: Mutex::new(None),
        supplies_id: true,
        lookalike: false,
        outputs: Mutex::new(None),
    });
    let mut wallet = joined(
        &w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
        &rail,
    )
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let device = store();
    let mut shop = Paykit::join(
        w.env(&w.bob, device.clone()),
        Default::default(),
        w.approve(&w.bob, "shop", &AppScopes::new().request().build().unwrap()),
        rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let earlier = shop.backup().await.unwrap();
    pay(&mut wallet, &w).await;
    shop.receive(None).await.unwrap();
    let key = shop.payee_activity().await.unwrap()[0].record;
    let proof_only = shop
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let receipt = shop.confirm_payment(&key).await.unwrap().unwrap();
    let signed = shop
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let id = rail
        .settlement_id(
            &crate::common::endpoint("btc-lightning-bolt12"),
            rail.last.lock().unwrap().as_ref().unwrap(),
        )
        .unwrap();
    let target = target(&paykit_lib::derive::natural_key::payee_payment_record(
        &endpoint("btc-lightning-bolt12"),
        &id,
    ));
    Payment {
        world: w,
        shop,
        wallet,
        device,
        rail,
        earlier,
        target,
        key,
        receipt,
        signed,
        proof_only,
    }
}

async fn restore(
    world: &World,
    device: &Arc<dyn StateStore>,
    backup: &PaykitBackup,
    rail: Arc<dyn Rail>,
) -> paykit_sdk::Result<Paykit> {
    Paykit::restore(
        world.env(&world.bob, device.clone()),
        Default::default(),
        backup,
        rail,
        world.timer(),
    )
    .await
}

#[tokio::test]
async fn test_payee_deletion_offline_custom_receipt_waits_for_its_method() {
    let Payment {
        world: w,
        shop,
        mut wallet,
        device,
        rail,
        earlier,
        target,
        key,
        receipt,
        proof_only,
        ..
    } = Box::pin(issue()).await;
    let mut seller_wallet = joined(
        &w,
        &w.bob,
        "seller-wallet",
        AppScopes::new().request().build().unwrap(),
        &rail,
    )
    .await;
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .put_record(&target, &proof_only, Vec::new())
        .await
        .unwrap();
    drop(shop);
    let mut unavailable = Box::pin(restore(&w, &device, &earlier, w.rail.clone()))
        .await
        .unwrap();
    assert!(unavailable
        .retained_payee_receipts()
        .await
        .unwrap()
        .is_empty());
    unavailable.receive(None).await.unwrap();
    assert!(unavailable
        .payee_deletion_notices()
        .await
        .unwrap()
        .is_empty());
    let backup = unavailable.backup().await.unwrap();
    drop(unavailable);
    let mut restored = Box::pin(restore(&w, &device, &backup, rail)).await.unwrap();
    let kept = restored.retained_payee_receipts().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].receipt.message().msg_id, receipt);
    assert_eq!(kept[0].record, key);
    assert!(kept[0].deletions.is_empty());
    restored.receive(None).await.unwrap();
    let writes = w.record_writes(&w.bob, &key);
    assert_eq!(restored.confirm_payment(&key).await.unwrap(), None);
    assert_eq!(w.record_writes(&w.bob, &key), writes);

    pay(&mut wallet, &w).await;
    restored.receive(None).await.unwrap();
    let next = restored
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.record != key)
        .unwrap()
        .record;
    assert!(restored.confirm_payment(&next).await.unwrap().is_some());
}

async fn receipt_row(shop: &Paykit) -> serde_json::Value {
    let rows = shop.app().protocol_scan("paykit", b"rc/").await.unwrap();
    assert_eq!(rows.len(), 1);
    let row: serde_json::Value = serde_json::from_slice(&rows[0].1).unwrap();
    row["d"].clone()
}

#[tokio::test]
async fn test_payee_deletion_unresolved_source_limit_preserves_the_installation() {
    let Payment {
        world: w,
        mut shop,
        device,
        rail,
        earlier,
        key,
        receipt,
        signed,
        ..
    } = Box::pin(issue()).await;
    let app_id = shop.app_id();
    for i in 1..8 {
        shop.app_mut()
            .put_record(&target(&[i]), &signed, Vec::new())
            .await
            .unwrap();
    }
    drop(shop);
    let mut pending = Box::pin(restore(&w, &device, &earlier, w.rail.clone()))
        .await
        .unwrap();
    let initial = receipt_row(&pending).await;
    assert_eq!(initial["source"]["pending"].as_array().unwrap().len(), 8);
    let weak = pending.backup().await.unwrap();
    drop(pending);
    let mut pending = Box::pin(restore(&w, &device, &weak, w.rail.clone()))
        .await
        .unwrap();
    assert_eq!(receipt_row(&pending).await, initial);
    let extra = target(&[8]);
    let extra_key = pending.app().record_key(&extra).unwrap();
    pending
        .app_mut()
        .put_record(&extra, &signed, Vec::new())
        .await
        .unwrap();
    drop(pending);
    let overflow = Box::pin(restore(&w, &device, &weak, w.rail.clone())).await;
    assert!(overflow
        .err()
        .unwrap()
        .to_string()
        .contains("too many source keys for a retained receipt"));
    let mut installed = Paykit::load(
        w.env(&w.bob, device.clone()),
        Default::default(),
        app_id,
        rail,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(receipt_row(&installed).await, initial);
    assert_eq!(
        installed
            .app()
            .shared_record(&extra_key)
            .await
            .unwrap()
            .unwrap()
            .content
            .value(),
        Some(signed.as_slice())
    );
    let kept = installed.retained_payee_receipts().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].record, key);
    assert_eq!(kept[0].receipt.message().msg_id, receipt);
    let bound = receipt_row(&installed).await;
    assert!(bound["source"]["bound"].is_object());
    drop(installed);
    let imported = Box::pin(restore(&w, &device, &weak, w.rail.clone()))
        .await
        .unwrap();
    assert_eq!(receipt_row(&imported).await, bound);
}

#[tokio::test]
async fn test_payee_deletion_a_misplaced_custom_receipt_does_not_count_as_credit() {
    let Payment {
        world: w,
        mut shop,
        device,
        rail,
        earlier,
        target: original,
        key,
        signed,
        proof_only,
        ..
    } = Box::pin(issue()).await;
    shop.app_mut()
        .put_record(&target(b"other activity"), &signed, Vec::new())
        .await
        .unwrap();
    shop.app_mut()
        .put_record(&original, &proof_only, Vec::new())
        .await
        .unwrap();
    drop(shop);
    let mut unavailable = Box::pin(restore(&w, &device, &earlier, w.rail.clone()))
        .await
        .unwrap();
    assert!(unavailable
        .retained_payee_receipts()
        .await
        .unwrap()
        .is_empty());
    unavailable.receive(None).await.unwrap();
    let weak = unavailable.backup().await.unwrap();
    drop(unavailable);
    let mut installed = Box::pin(restore(&w, &device, &weak, rail)).await.unwrap();
    assert!(installed
        .retained_payee_receipts()
        .await
        .unwrap()
        .is_empty());
    assert!(installed
        .app()
        .protocol_scan("paykit", b"rc/")
        .await
        .unwrap()
        .is_empty());
    // The permanent mark belongs to the original credit. The misplaced receipt
    // neither supplies its missing signed receipt nor authorizes another credit.
    let before = w.record_writes(&w.bob, &key);
    assert_eq!(installed.confirm_payment(&key).await.unwrap(), None);
    let buckets = installed
        .app()
        .records_of(Collection::Settlements.name())
        .await
        .unwrap();
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0].policy, Collection::Settlements.fixed_policy());
    assert_eq!(w.record_writes(&w.bob, &key), before);
}
