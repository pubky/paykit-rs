use super::*;
use paykit_sdk::Refusal;
use pubky_switch_client::app::AppEvent;

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
    world.add_switch_with(origin, &[&identity], limits).await?;
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
