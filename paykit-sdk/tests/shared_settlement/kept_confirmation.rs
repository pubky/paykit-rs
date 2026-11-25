use super::*;
use paykit_sdk::Refusal;
use pubky_dispatch_client::app::{AppEvent, UserConfirmed};

/// A confirmation of a held request, kept in an attempt's batch whose answer was lost and
/// applied only after a newer `missed` item, confirms nothing: on both services the request
/// stays held, and neither an automatic nor a manual payment without a new confirmation goes.
#[tokio::test]
async fn test_enrolled_kept_hold_confirmation_confirms_no_newer_missed_item() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(kept_confirmation(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

/// The first grant of an Allowance, kept in a batch whose answer was lost and applied only
/// after a `missed` item, consents nothing on both services: the Allowance waits for the
/// user to confirm it again, and no resume alone consents it.
#[tokio::test]
async fn test_enrolled_kept_first_grant_consents_nothing_after_a_missed_item() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(kept_first_grant(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

/// The first grant's batch applies at the service and its answer is lost. The host retries
/// the resume with no read in between, on both services: the retry sends the kept batch
/// again and consents, with no second grant.
#[tokio::test]
async fn test_enrolled_resume_retried_without_a_read_lands_a_kept_grant() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(retried_grant(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

/// A one-time request from `shop` to `payer`, created now.
fn one_time(world: &World, reference: &str) -> TestResult<RequestDraft> {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
        PaymentEndpointPayload::new(&format!("invoice-{reference}"))?,
    )]);
    Ok(RequestDraft::new(
        PaymentAmount {
            value: Decimal::new("1")?,
            asset: Asset::new("btc")?,
        },
        PaymentReference::new(reference)?,
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(world.now_ms() + 24 * 3_600_000))
}

/// While the payer's wallet is away, the shop's messages fill the small inbox the payer's
/// phone drains, so the wallet's next read reports a `missed` item.
async fn missed_while_away(
    world: &World,
    shop: &mut Paykit,
    phone: &mut Paykit,
    signer: &mut Signer,
    payer: [u8; 32],
    round: usize,
) -> TestResult {
    for index in 0..8 {
        shop.send_request(payer, one_time(world, &format!("filler-{round}-{index}"))?)
            .await?;
        phone.receive(None).await?;
        phone.app_mut().flush_cursor().await?;
        if index == 3 || index == 7 {
            signer.run_due().await?;
        }
        world.advance_ms(61_250);
    }
    Ok(())
}

/// Whether `wallet` holds its first grant's checkpoint (`lv/`) for the Allowance at
/// `record`: the local write of the grant's batch, applied when the batch is.
async fn granted(wallet: &Paykit, record: &[u8; 32]) -> TestResult<bool> {
    let key = format!("lv/{}", pubky_dispatch_core::enc::b64(record));
    Ok(wallet
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await?
        .is_some())
}

async fn confirmations(wallet: &Paykit) -> TestResult<usize> {
    Ok(wallet.app().protocol_scan("paykit", b"hc/").await?.len())
}

fn reports_missed(report: &paykit_sdk::ReceiveSummary) -> bool {
    report
        .report
        .events
        .iter()
        .any(|event| matches!(event, AppEvent::Missed { .. }))
}

async fn kept_confirmation(world: &World) -> TestResult {
    let origin = "https://payer.payment.test";
    let identity = world.identity("payer")?;
    let limits = serde_json::from_value(serde_json::json!({
        "quota_bytes": 8192, "sender_share_percent": 100
    }))?;
    world
        .add_dispatch_with(origin, &[&identity], limits)
        .await?;
    let mut payer_signer = Box::pin(signer_on(world, "payer", origin)).await?;
    let mut shop_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut wallet = Box::pin(join(
        world,
        &mut payer_signer,
        "payer",
        "payer-wallet",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut phone = Box::pin(join(
        world,
        &mut payer_signer,
        "payer",
        "payer-phone",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "shop-wallet",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    wallet.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&wallet.identity()).await?;
    let request = shop
        .send_request(wallet.identity(), one_time(world, "order-1")?)
        .await?
        .request
        .ok_or("the request is accepted at once")?;
    wallet.receive(None).await?;
    phone.receive(None).await?;
    phone.app_mut().flush_cursor().await?;

    Box::pin(missed_while_away(
        world,
        &mut shop,
        &mut phone,
        &mut payer_signer,
        wallet.identity(),
        1,
    ))
    .await?;
    let report = wallet.receive(None).await?;
    assert!(reports_missed(&report), "{:?}", report.report.events);
    let confirmed = wallet
        .request(&request)
        .await?
        .held
        .ok_or("the missed item holds the request")?;
    // The attempt's batch is applied, its answer lost: the client keeps it.
    world.lose_answer("POST", "/ops");
    let lost = wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .outcome;
    assert!(lost.is_err(), "{lost:?}");
    assert!(rail.all_executions().is_empty());

    Box::pin(missed_while_away(
        world,
        &mut shop,
        &mut phone,
        &mut payer_signer,
        wallet.identity(),
        2,
    ))
    .await?;
    // The kept batch can't be sent while the newer item is read.
    world.unreachable("POST", "/ops");
    let report = wallet.receive(None).await?;
    assert!(reports_missed(&report), "{:?}", report.report.events);
    assert_eq!(
        confirmations(&wallet).await?,
        0,
        "the newer item cleared them"
    );
    wallet.receive(None).await?;
    assert_eq!(
        confirmations(&wallet).await?,
        1,
        "the kept batch's confirmation landed after the newer item"
    );

    assert!(wallet.request(&request).await?.held.is_some());
    for automatic in [true, false] {
        let refused = if automatic {
            wallet
                .pay_request_automatically(&request, PayOptions::default())
                .await
        } else {
            wallet.pay_request(&request, PayOptions::default()).await
        }
        .outcome
        .unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::HeldAfterRestore { .. })),
            "{refused:?}"
        );
    }
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn kept_first_grant(world: &World) -> TestResult {
    let origin = "https://payer.payment.test";
    let identity = world.identity("payer")?;
    let limits = serde_json::from_value(serde_json::json!({
        "quota_bytes": 8192, "sender_share_percent": 100
    }))?;
    world
        .add_dispatch_with(origin, &[&identity], limits)
        .await?;
    let mut payer_signer = Box::pin(signer_on(world, "payer", origin)).await?;
    let mut shop_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut wallet = Box::pin(join(
        world,
        &mut payer_signer,
        "payer",
        "payer-wallet",
        AppScopes::new()
            .allowance_execute()
            .manage_contacts()
            .build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut phone = Box::pin(join(
        world,
        &mut payer_signer,
        "payer",
        "payer-phone",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "shop-wallet",
        AppScopes::new()
            .allowance()
            .request()
            .manage_contacts()
            .build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    wallet.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&wallet.identity()).await?;
    let terms = paykit_lib::allowances::AllowanceTerms::from_json(&serde_json::json!({
        "asset": "btc", "per_payment_amount": null, "period_limits": [],
        "lifetime_amount_limit": "100", "active_from": null, "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))?;
    let offer =
        Box::pin(wallet.propose_executable_allowance(shop.identity(), terms, UserConfirmed::new()))
            .await?;
    Box::pin(shop.receive(None)).await?;
    let at_shop = shop
        .allowances()
        .await?
        .into_iter()
        .find(|v| v.allowance_id == offer.allowance_id)
        .ok_or("the proposal reached the shop")?;
    Box::pin(shop.accept_allowance(&at_shop.record)).await?;
    Box::pin(wallet.receive(None)).await?;
    phone.receive(None).await?;
    phone.app_mut().flush_cursor().await?;

    // The grant's batch is applied, its answer lost: the client keeps it.
    world.lose_answer("POST", "/ops");
    let lost = Box::pin(wallet.resume_allowance_executor(&offer.record)).await;
    assert!(lost.is_err(), "{lost:?}");
    Box::pin(missed_while_away(
        world,
        &mut shop,
        &mut phone,
        &mut payer_signer,
        wallet.identity(),
        1,
    ))
    .await?;
    // The kept batch can't be sent while the item is read.
    world.unreachable("POST", "/ops");
    let report = Box::pin(wallet.receive(None)).await?;
    assert!(reports_missed(&report), "{:?}", report.report.events);
    assert!(
        !granted(&wallet, &offer.record).await?,
        "the grant's batch is still kept"
    );
    Box::pin(wallet.receive(None)).await?;
    assert!(
        granted(&wallet, &offer.record).await?,
        "the kept batch landed after the missed item"
    );
    // The shared records count as listed again from the next receive's listing.
    Box::pin(wallet.receive(None)).await?;
    let view = wallet.allowance(&offer.record).await?;
    assert_eq!(
        view.executor_app_id,
        Some(wallet.app_id()),
        "the kept grant applied"
    );
    assert!(!view.consented, "{view:?}");
    let resumed = Box::pin(wallet.resume_allowance_executor(&offer.record)).await;
    assert!(resumed.is_err(), "{resumed:?}");
    assert!(!wallet.allowance(&offer.record).await?.consented);
    Box::pin(wallet.confirm_allowance_executor(
        &wallet.allowance(&offer.record).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await?;
    assert!(wallet.allowance(&offer.record).await?.consented);
    Ok(())
}

async fn retried_grant(world: &World) -> TestResult {
    let mut payer_signer =
        Box::pin(setup_identity(world, "payer", "https://payer.payment.test")).await?;
    let mut shop_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut wallet = Box::pin(join(
        world,
        &mut payer_signer,
        "payer",
        "payer-wallet",
        AppScopes::new()
            .allowance_execute()
            .manage_contacts()
            .build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "shop-wallet",
        AppScopes::new()
            .allowance()
            .request()
            .manage_contacts()
            .build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    wallet.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&wallet.identity()).await?;
    let terms = paykit_lib::allowances::AllowanceTerms::from_json(&serde_json::json!({
        "asset": "btc", "per_payment_amount": null, "period_limits": [],
        "lifetime_amount_limit": "100", "active_from": null, "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))?;
    let offer =
        Box::pin(wallet.propose_executable_allowance(shop.identity(), terms, UserConfirmed::new()))
            .await?;
    Box::pin(shop.receive(None)).await?;
    let at_shop = shop
        .allowances()
        .await?
        .into_iter()
        .find(|v| v.allowance_id == offer.allowance_id)
        .ok_or("the proposal reached the shop")?;
    Box::pin(shop.accept_allowance(&at_shop.record)).await?;
    Box::pin(wallet.receive(None)).await?;

    world.lose_answer("POST", "/ops");
    let lost = Box::pin(wallet.resume_allowance_executor(&offer.record)).await;
    assert!(lost.is_err(), "{lost:?}");
    assert!(!granted(&wallet, &offer.record).await?);
    Box::pin(wallet.resume_allowance_executor(&offer.record)).await?;
    assert!(granted(&wallet, &offer.record).await?);
    let view = wallet.allowance(&offer.record).await?;
    assert!(view.consented, "{view:?}");
    assert_eq!(view.executor_app_id, Some(wallet.app_id()));
    Ok(())
}
