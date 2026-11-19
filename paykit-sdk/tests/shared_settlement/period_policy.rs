use super::*;

#[tokio::test]
async fn test_two_periods_under_app_policy_and_a_missed_period_end_at_cancellation() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        for restore in [false, true] {
            let Some(world) = World::start(backend)? else {
                continue;
            };
            let result = Box::pin(subscription_through_cancellation(&world, restore)).await;
            world.close().await?;
            result?;
        }
    }
    Ok(())
}

async fn subscription_through_cancellation(world: &World, restore: bool) -> TestResult {
    use paykit_lib::bodies::PeriodDetails;
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
    use paykit_sdk::{Refusal, RequestStatus};
    use pubky_switch_client::app::{AppEvent, UserConfirmed};

    // The subscriber and a second enrolled wallet share a small service inbox. Only the
    // second wallet reads the period details while the subscriber is away.
    let origin = "https://subscriber.payment.test";
    let identity = world.identity("subscriber")?;
    let limits = serde_json::from_value(serde_json::json!({
        "quota_bytes": 8192, "sender_share_percent": 100
    }))?;
    world.add_switch_with(origin, &[&identity], limits).await?;
    let mut buyer_signer = Box::pin(signer_on(world, "subscriber", origin)).await?;
    let mut shop_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut store = Arc::new(MemoryStore::new());
    let mut buyer = Box::pin(join(
        world,
        &mut buyer_signer,
        "subscriber",
        "subscriber-app",
        AppScopes::new().pay().manage_contacts().build()?,
        store.clone(),
        &rail,
    ))
    .await?;
    let mut phone = Box::pin(join(
        world,
        &mut buyer_signer,
        "subscriber",
        "subscriber-phone",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "period-till",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Hour)?, start, None)?;
    let periods: Vec<_> = recurrence
        .periods(3)?
        .into_iter()
        .map(PeriodRef::from)
        .collect();
    let request = Box::pin(subscribe(world, &mut buyer, &mut shop, recurrence, false)).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(request.request_hash))
        .ok_or("missing subscription at the seller")?
        .record;
    phone.receive(None).await?;
    if restore {
        let backup = buyer.backup().await?;
        drop(buyer);
        store = Arc::new(MemoryStore::new());
        buyer = Box::pin(Paykit::restore(
            world.app_env(store.clone()),
            Default::default(),
            &backup,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        buyer.receive(None).await?;
        let report = Box::pin(buyer.reconcile()).await?;
        assert!(report.doubtful.is_empty(), "{report:?}");
        buyer.end_reconcile(UserConfirmed::new()).await?;
        if let Some(held) = buyer.request(&request).await?.held {
            buyer
                .confirm_possibly_cancelled(&request, &held, UserConfirmed::new())
                .await?;
        }
    }
    assert!(rail.all_executions().is_empty());
    let mut paid = Vec::new();
    for &period in &periods[..2] {
        world.advance_ms(period.starts_at.saturating_sub(world.now_ms()));
        if paid.is_empty() {
            rail.set_declaration(
                paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
                    .with_idempotent(true),
            );
            rail.next(paykit_sdk::rail::Behaviour::Hang);
        }
        let (returned, result) = dispatch_period(buyer, request, period).await?;
        buyer = returned;
        let attempt_id = if paid.is_empty() {
            let PayOutcome::Pending { attempt_id, .. } = result.outcome? else {
                return Err("first dispatch did not stay in flight".into());
            };
            let app_id = buyer.app_id();
            drop(buyer);
            phone.receive(None).await?;
            rail.settle(&attempt_id, true);
            world.advance_ms(60_000);
            buyer = Box::pin(Paykit::load(
                world.app_env(store.clone()),
                Default::default(),
                app_id,
                rail.clone(),
                Arc::new(TestTimer),
            ))
            .await?;
            buyer.receive(None).await?;
            assert!(!buyer.request(&request).await?.manual_only);
            let (returned, outcomes) = tokio::spawn(Box::pin(async move {
                let outcomes = buyer.resume_payments().await;
                (buyer, outcomes)
            }))
            .await?;
            buyer = returned;
            let outcomes = outcomes?;
            assert!(
                matches!(outcomes.as_slice(), [PayOutcome::Paid { attempt_id: original, .. }] if *original == attempt_id),
                "{outcomes:?}"
            );
            assert_eq!(rail.pay_calls(&attempt_id), 2);
            attempt_id
        } else {
            let PayOutcome::Paid { attempt_id, .. } = result.outcome? else {
                return Err("second period did not settle".into());
            };
            assert_eq!(rail.pay_calls(&attempt_id), 1);
            attempt_id
        };
        shop.receive(None).await?;
        let activity = shop
            .payee_activity()
            .await?
            .into_iter()
            .find(|activity| activity.record == sale)
            .ok_or("missing seller activity")?;
        let settled = activity
            .periods
            .iter()
            .find(|item| item.billing_period == period)
            .ok_or("missing seller period")?;
        assert!(settled.paid && !settled.credited);
        let receipt = Box::pin(shop.confirm_payment(&settled.record))
            .await?
            .ok_or("missing signed receipt")?;
        buyer.receive(None).await?;
        phone.receive(None).await?;
        let receipts = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].attempt_id, attempt_id);
        assert_eq!(receipts[0].msg_id, receipt);
        assert_eq!(rail.executions(&attempt_id), 1);
        paid.push((period, attempt_id, receipt));
        buyer.app_mut().flush_cursor().await?;
        phone.app_mut().flush_cursor().await?;
    }
    assert_ne!(paid[0].1, paid[1].1);
    assert_ne!(paid[0].2, paid[1].2);
    let third = periods[2];
    world.advance_ms(third.starts_at.saturating_sub(world.now_ms()));
    for index in 0..8 {
        let endpoints = BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new(&format!("third-period-{index}"))?,
        )]);
        Box::pin(shop.send_period_details(
            &sale,
            PeriodDetails {
                billing_period: third,
                payment_endpoints: Some(endpoints),
                rates: None,
                expires_at: third.ends_at,
            },
        ))
        .await?;
        phone.receive(None).await?;
        phone.app_mut().flush_cursor().await?;
        if index == 3 || index == 7 {
            buyer_signer.run_due().await?;
        }
        world.advance_ms(61_250);
    }
    let report = buyer.receive(None).await?;
    assert!(
        report
            .report
            .events
            .iter()
            .any(|event| matches!(event, AppEvent::Missed { .. })),
        "{:?}",
        report.report.events
    );
    let (returned, withheld) = dispatch_period(buyer, request, third).await?;
    buyer = returned;
    let withheld = withheld.outcome.unwrap_err();
    assert!(
        matches!(withheld.refusal(), Some(Refusal::HeldAfterRestore { .. })),
        "{withheld:?}"
    );
    assert_eq!(rail.all_executions().len(), 2);
    Box::pin(shop.cancel_sent_request(&sale, None)).await?;
    buyer.receive(None).await?;
    assert_eq!(
        buyer.request(&request).await?.status,
        RequestStatus::Cancelled
    );
    let (returned, stopped) = dispatch_period(buyer, request, third).await?;
    buyer = returned;
    let stopped = stopped.outcome.unwrap_err();
    assert!(
        matches!(stopped.refusal(), Some(Refusal::Cancelled)),
        "{stopped:?}"
    );
    assert!(buyer
        .receipts(ReceiptsOf::Period(request, third))
        .await?
        .is_empty());
    let seller = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|activity| activity.record == sale)
        .ok_or("seller lost the subscription")?;
    assert_eq!(
        seller
            .periods
            .iter()
            .filter(|period| period.credited)
            .count(),
        2
    );
    for (period, attempt, receipt) in paid {
        let receipts = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].attempt_id, attempt);
        assert_eq!(receipts[0].msg_id, receipt);
        let signed = seller
            .periods
            .iter()
            .find(|view| view.billing_period == period)
            .and_then(|view| view.receipt.as_ref())
            .ok_or("seller lost its issued receipt")?;
        assert_eq!(signed.attempt_id, attempt);
        assert_eq!(signed.msg_id, receipt);
    }
    assert_eq!(rail.all_executions().values().sum::<u64>(), 2);
    Ok(())
}

async fn dispatch_period(
    mut buyer: Paykit,
    request: RequestRef,
    period: PeriodRef,
) -> TestResult<(Paykit, paykit_sdk::PayResult)> {
    Ok(tokio::spawn(Box::pin(async move {
        let payment = buyer.pay_period_automatically(&request, &period).await;
        (buyer, payment)
    }))
    .await?)
}
