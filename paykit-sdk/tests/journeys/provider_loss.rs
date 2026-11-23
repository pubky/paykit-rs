//! After a provider loss: every wallet of the payer's identity holds automatic one-time
//! payments for good and tells its host, and a payment the user starts by hand still goes
//! once (Paykit spec 8.1, Blocked counterparties; Paykit wire 5.6, Shared post-loss hold).

use std::sync::Arc;

use paykit_sdk::{AppScopes, NoticeKind, PayOptions, PayOutcome, Paykit, PaykitConfig, Refusal};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::store::MemoryStore;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use pubky_switch_harness::World;

use super::{checkout, finish_listing, invoice, on_both_backends, Checkout, TestResult};

/// Where the payer's signer recovers the account.
const RECOVERY_ORIGIN: &str = "https://payer-recovery.journey.test";

#[tokio::test]
async fn test_provider_loss_holds_automatic_one_time_payments_on_every_wallet() -> TestResult {
    on_both_backends(held_after_loss).await
}

/// Follow the declared loss: the newer `identity.json`, the reads at the new service and the
/// payment reconciliation the loss owes. Returns the end of its residual window.
async fn follow(app: &mut Paykit) -> TestResult<u64> {
    Box::pin(app.app_mut().refresh_identity()).await?;
    for _ in 0..4 {
        Box::pin(app.receive(None)).await?;
    }
    Box::pin(finish_listing(app)).await?;
    let mut reconciled = Box::pin(app.reconcile()).await;
    for _ in 0..4 {
        if reconciled.is_ok() {
            break;
        }
        Box::pin(app.receive(None)).await?;
        reconciled = Box::pin(app.reconcile()).await;
    }
    reconciled?;
    Box::pin(app.end_reconcile(UserConfirmed::new())).await?;
    let info = Box::pin(app.app().recovery_info())
        .await?
        .ok_or("the wallet follows no provider loss")?;
    Ok(info.loss.uncertain_until_ms)
}

async fn held_after_loss(world: &World) -> TestResult {
    let Checkout {
        mut payer,
        mut wallet,
        mut shop,
        rail,
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let mut phone = Box::pin(payer.join(
        world,
        "phone",
        AppScopes::new().pay().build()?,
        PaykitConfig::default(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;

    // A request before the loss makes the seller a counterparty the loss's steps carry to
    // the new service.
    Box::pin(shop.send_request(
        wallet.identity(),
        invoice("0.0001", "order-before-loss", "lnbc-order-before-loss")?,
    ))
    .await?;
    for app in [&mut wallet, &mut phone] {
        Box::pin(app.receive(None)).await?;
    }

    // The payer's service is lost, and its signer recovers the account at another.
    world.lose_switch("https://payer.journey.test");
    let identity = world.identity("payer")?;
    Box::pin(world.add_switch(RECOVERY_ORIGIN, &[&identity])).await?;
    let doc = Box::pin(payer.signer.declare_provider_loss(
        RECOVERY_ORIGIN,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    assert!(doc.body().recovery_from.is_some());
    world.advance_ms(1_000);
    let mut until = 0;
    for app in [&mut wallet, &mut phone] {
        until = until.max(Box::pin(follow(app)).await?);
    }
    // Past the residual window, so no doubt of an obligation from before the loss holds
    // the request: the shared hold alone does.
    world.advance_ms(until.saturating_sub(world.now_ms()) + 2 * CLOCK_TOLERANCE_MS);

    let sent = Box::pin(shop.send_request(
        wallet.identity(),
        invoice("0.00025", "order-after-loss", "lnbc-order-after-loss")?,
    ))
    .await?;
    let request = sent.request.ok_or("the request was not sent")?;
    for app in [&mut wallet, &mut phone] {
        Box::pin(app.receive(None)).await?;
        let refused = Box::pin(app.pay_request_automatically(&request, PayOptions::default()))
            .await
            .outcome
            .err()
            .ok_or("paid automatically after the provider loss")?;
        assert!(
            matches!(refused.refusal(), Some(Refusal::NotAutomatic(_))),
            "{refused}"
        );
        let told = Box::pin(app.notices()).await?;
        assert_eq!(
            told.iter()
                .filter(|notice| notice.kind == NoticeKind::AutomaticPaymentsHeld)
                .count(),
            1,
            "{told:?}"
        );
    }
    assert!(rail.all_pay_calls().is_empty());

    // A wallet that joins after the loss follows no loss and holds none recorded: the newest
    // own `identity.json` it verified, whose `recovery_from` stays set, holds it alone.
    let mut laptop = Box::pin(payer.join_unlisted(
        world,
        "laptop",
        AppScopes::new().pay().build()?,
        PaykitConfig::default(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let told = Box::pin(laptop.notices()).await?;
    assert_eq!(
        told.iter()
            .filter(|notice| notice.kind == NoticeKind::AutomaticPaymentsHeld)
            .count(),
        1,
        "told at the join: {told:?}"
    );
    Box::pin(super::finish_listing(&mut laptop)).await?;
    for _ in 0..2 {
        Box::pin(laptop.receive(None)).await?;
    }
    assert!(laptop.app().own_identity().body().recovery_from.is_some());
    assert!(Box::pin(laptop.app().recovery_info()).await?.is_none());
    let refused = Box::pin(laptop.pay_request_automatically(&request, PayOptions::default()))
        .await
        .outcome
        .err()
        .ok_or("a wallet joined after the loss paid automatically")?;
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotAutomatic(_))),
        "{refused}"
    );
    assert!(rail.all_pay_calls().is_empty());

    // By hand, it goes once.
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(rail.all_pay_calls().values().sum::<u64>(), 1);
    Ok(())
}
