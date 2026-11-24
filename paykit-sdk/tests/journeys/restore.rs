//! A wallet backed up and restored into a fresh store in the middle of a one-time request,
//! which then goes on to its receipt (Paykit spec 7.2, "What a restore loses"; 7.3).
//!
//! After a clean restore, from a backup of the device's latest state, the app resumes and
//! holds no request: the payer's after the request reached it, the seller's after the proof
//! reached it. After any other restore, as when the old device kept reading after its
//! backup, every open request is held until the user has seen it may have been cancelled.

use std::sync::Arc;

use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    PayOptions, PayeeStatus, Paykit, PaykitBackup, PaykitConfig, Refusal, RequestRef, RequestStatus,
};
use pubky_switch_client::app::{HeldCursor, UserConfirmed};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_harness::World;

use super::{
    app_env, btc, checkout, endpoint, invoice, on_both_backends, paid_and_receipted, Checkout,
    Expected, HostTimer, TestResult,
};

#[tokio::test]
async fn test_restore_clean_payer_pays_a_request_received_before_its_backup() -> TestResult {
    on_both_backends(payer_restored_clean).await
}

#[tokio::test]
async fn test_restore_clean_seller_receipts_a_proof_received_before_its_backup() -> TestResult {
    on_both_backends(seller_restored_clean).await
}

#[tokio::test]
async fn test_restore_behind_the_old_device_holds_the_request_until_the_user_confirms() -> TestResult
{
    on_both_backends(payer_restored_behind).await
}

/// Restore `backup` into a fresh store, as a host does on a new device, and read the inbox.
pub(super) async fn restore(
    world: &World,
    backup: &PaykitBackup,
    rail: &Arc<FakeRail>,
) -> TestResult<Paykit> {
    let mut restored = Box::pin(Paykit::restore(
        app_env(world, Arc::new(MemoryStore::new())),
        PaykitConfig::default(),
        backup,
        rail.clone(),
        Arc::new(HostTimer),
    ))
    .await?;
    Box::pin(restored.receive(None)).await?;
    Ok(restored)
}

/// The restored app reconciles, finding nothing in doubt, its payer pass run only for a
/// payer, and ends reconciling.
pub(super) async fn reconciled(app: &mut Paykit, payer: bool) -> TestResult {
    let report = Box::pin(app.reconcile()).await?;
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert_eq!(report.payer_skipped, !payer, "{report:?}");
    Box::pin(app.end_reconcile(UserConfirmed::new())).await?;
    Ok(())
}

/// Back `app` up at its latest state, drop it, and restore it: the restore is clean, the
/// user has no registrations to account for, and it reconciles.
async fn restored_clean(
    world: &World,
    mut app: Paykit,
    rail: &Arc<FakeRail>,
    payer: bool,
) -> TestResult<Paykit> {
    let app_id = app.app_id();
    let backup = Box::pin(app.backup()).await?;
    drop(app);
    let mut restored = Box::pin(restore(world, &backup, rail)).await?;
    assert_eq!(restored.app_id(), app_id);
    assert_eq!(
        restored.app().restore_held_cursor(),
        Some(HeldCursor::Clean)
    );
    assert!(restored.app().restore_question().is_empty());
    Box::pin(reconciled(&mut restored, payer)).await?;
    Ok(restored)
}

/// The seller sends a request for `value` btc; the payer's wallet receives it and shows it
/// open.
async fn requested(wallet: &mut Paykit, shop: &mut Paykit, value: &str) -> TestResult<RequestRef> {
    let draft = invoice(value, "order-restore", "lnbc-order-restore")?;
    let sent = Box::pin(shop.send_request(wallet.identity(), draft)).await?;
    let request = sent.request.ok_or("the request was not sent")?;
    let read = Box::pin(wallet.receive(None)).await?;
    assert!(read.applied.contains(&sent.msg_id), "{read:?}");
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Open
    );
    Ok(request)
}

fn expected(rail: &FakeRail, request: &RequestRef) -> TestResult<Expected> {
    Ok(Expected::new(
        rail,
        Some(request.request_hash),
        btc("0.0004")?,
        endpoint("btc-lightning-bolt11")?,
    ))
}

async fn payer_restored_clean(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let request = Box::pin(requested(&mut wallet, &mut shop, "0.0004")).await?;
    let mut wallet = Box::pin(restored_clean(world, wallet, &rail, true)).await?;

    // The restored wallet still holds the request open, pays it once, and gets its receipt.
    let view = Box::pin(wallet.request(&request)).await?;
    assert_eq!(view.status, RequestStatus::Open, "{view:?}");
    assert!(view.held.is_none(), "{view:?}");
    assert!(Box::pin(wallet.payments()).await?.is_empty());
    let expected = expected(&rail, &request)?;
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected,
    ))
    .await?;
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}

async fn seller_restored_clean(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let request = Box::pin(requested(&mut wallet, &mut shop, "0.0004")).await?;
    let expected = expected(&rail, &request)?;
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    Box::pin(shop.receive(None)).await?;
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);

    // The restored seller still holds the proof, and verifies, confirms and receipts it.
    let mut shop = Box::pin(restored_clean(world, shop, &rail, false)).await?;
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].request_hash, Some(request.request_hash));
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected,
    ))
    .await?;
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}

async fn payer_restored_behind(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let request = Box::pin(requested(&mut wallet, &mut shop, "0.0004")).await?;
    let seller = shop.identity();

    // The wallet is backed up, then keeps reading: a second request reaches it after the
    // backup. Then it is lost, and restored from that backup.
    let backup = Box::pin(wallet.backup()).await?;
    Box::pin(requested(&mut wallet, &mut shop, "0.0009")).await?;
    drop(wallet);
    let mut wallet = Box::pin(restore(world, &backup, &rail)).await?;
    let verdict = wallet.app().restore_held_cursor();
    assert!(
        matches!(verdict, Some(HeldCursor::Ahead { .. })),
        "{verdict:?}"
    );
    Box::pin(reconciled(&mut wallet, true)).await?;

    // The request reached the wallet before the backup and is still unpaid, but it is held:
    // the seller may have cancelled it since. Paying it is refused, and nothing is paid.
    let view = Box::pin(wallet.request(&request)).await?;
    assert_eq!(view.status, RequestStatus::Held, "{view:?}");
    let held = view.held.ok_or("the held request shows no hold")?;
    let refused = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome
        .err()
        .ok_or("a held request was paid")?;
    assert!(
        matches!(refused.refusal(), Some(Refusal::HeldAfterRestore { .. })),
        "{refused:?}"
    );
    assert!(rail.all_executions().is_empty());
    assert!(Box::pin(wallet.payments()).await?.is_empty());

    // The wallet asked the seller to send again; the seller reads the ask.
    let sent = Box::pin(wallet.app().sent_to(&seller, 0, u64::MAX)).await?;
    let asks: Vec<_> = sent
        .iter()
        .filter(|message| message.kind == "paykit.resend_request")
        .collect();
    assert_eq!(asks.len(), 1, "{sent:?}");
    let read = Box::pin(shop.receive(None)).await?;
    assert!(read.applied.contains(&asks[0].msg_id), "{read:?}");
    Box::pin(wallet.receive(None)).await?;
    let after = Box::pin(wallet.request(&request)).await?;
    // The seller's answer doesn't lift the hold: only the user does.
    assert_eq!(after.status, RequestStatus::Held, "{after:?}");
    assert!(rail.all_executions().is_empty());

    // The user has seen the seller may have cancelled it, and pays it anyway: it is paid
    // once and receipted.
    let expected = expected(&rail, &request)?;
    let options = PayOptions::default().with_possibly_cancelled_confirmed(held);
    let outcome = Box::pin(wallet.pay_request(&request, options))
        .await
        .outcome?;
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected,
    ))
    .await?;
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}
