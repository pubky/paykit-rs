//! A wallet backed up and restored into a fresh store in the middle of a one-time request,
//! which then goes on to its receipt: the payer's after the request reached it, the
//! seller's after the proof reached it (Paykit spec 7.3).

use std::sync::Arc;

use paykit_sdk::rail::FakeRail;
use paykit_sdk::{PayOptions, PayeeStatus, Paykit, PaykitConfig, RequestRef, RequestStatus};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::store::MemoryStore;
use pubky_switch_harness::World;

use super::{
    app_env, btc, checkout, endpoint, invoice, on_both_backends, paid_and_receipted, Checkout,
    Expected, HostTimer, TestResult,
};

#[tokio::test]
async fn test_restore_payer_pays_a_request_received_before_its_backup() -> TestResult {
    on_both_backends(payer_restored_before_paying).await
}

#[tokio::test]
async fn test_restore_seller_receipts_a_proof_received_before_its_backup() -> TestResult {
    on_both_backends(seller_restored_before_receipting).await
}

/// Back `app` up, drop it, and restore it into a fresh store, as a host does on a new
/// device; the restored app reads its inbox, reconciles and ends reconciling, finding
/// nothing in doubt.
async fn restored(world: &World, mut app: Paykit, rail: &Arc<FakeRail>) -> TestResult<Paykit> {
    let app_id = app.app_id();
    let backup = Box::pin(app.backup()).await?;
    drop(app);
    let mut restored = Box::pin(Paykit::restore(
        app_env(world, Arc::new(MemoryStore::new())),
        PaykitConfig::default(),
        &backup,
        rail.clone(),
        Arc::new(HostTimer),
    ))
    .await?;
    assert_eq!(restored.app_id(), app_id);
    Box::pin(restored.receive(None)).await?;
    let report = Box::pin(restored.reconcile()).await?;
    assert!(report.doubtful.is_empty(), "{report:?}");
    Box::pin(restored.end_reconcile(UserConfirmed::new())).await?;
    Ok(restored)
}

/// The seller sends a request; the payer's wallet receives it and shows it open.
async fn requested(wallet: &mut Paykit, shop: &mut Paykit) -> TestResult<RequestRef> {
    let draft = invoice("0.0004", "order-restore", "lnbc-order-restore")?;
    let sent = Box::pin(shop.send_request(wallet.identity(), draft)).await?;
    let request = sent.request.ok_or("the request was not sent")?;
    Box::pin(wallet.receive(None)).await?;
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Open
    );
    Ok(request)
}

fn expected(request: &RequestRef) -> TestResult<Expected> {
    Ok(Expected::new(
        Some(request.request_hash),
        btc("0.0004")?,
        endpoint("btc-lightning-bolt11")?,
    ))
}

async fn payer_restored_before_paying(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let request = Box::pin(requested(&mut wallet, &mut shop)).await?;
    let mut wallet = Box::pin(restored(world, wallet, &rail)).await?;

    // The restored wallet still holds the request open, pays it once, and gets its receipt.
    let view = Box::pin(wallet.request(&request)).await?;
    assert_eq!(view.status, RequestStatus::Open, "{view:?}");
    assert!(view.held.is_none(), "{view:?}");
    assert!(Box::pin(wallet.payments()).await?.is_empty());
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected(&request)?,
    ))
    .await?;
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}

async fn seller_restored_before_receipting(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let request = Box::pin(requested(&mut wallet, &mut shop)).await?;
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    Box::pin(shop.receive(None)).await?;
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);

    // The restored seller still holds the proof, and verifies, confirms and receipts it.
    let mut shop = Box::pin(restored(world, shop, &rail)).await?;
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].request_hash, Some(request.request_hash));
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected(&request)?,
    ))
    .await?;
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}
