//! A labelled subscription can also be handled by other authorized tills of its seller.

use super::*;
use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
use paykit_lib::scopes::Label;

#[tokio::test]
async fn test_labelled_sale_accepts_sibling_details_receipt_and_cancellation() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(labelled_sale(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn labelled_sale(world: &World) -> TestResult {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let (mut buyer, mut owner, mut seller) =
        Box::pin(period_wallets(world, Arc::new(MemoryStore::new()), &rail)).await?;
    let label = Label::new("subscriptions")?;
    let mut till = Box::pin(join(
        world,
        &mut seller,
        "shop",
        "subscription-till",
        AppScopes::new().request_label(&label).build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut counter = Box::pin(join(
        world,
        &mut seller,
        "shop",
        "subscription-counter",
        AppScopes::new().request_label(&label).build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    assert_ne!(till.app_id(), counter.app_id());
    assert_ne!(till.app_id(), owner.app_id());
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Month)?, start, None)?;
    let period = PeriodRef::from(recurrence.first()?);
    let request = Box::pin(subscribe(
        world,
        &mut buyer,
        &mut till,
        recurrence,
        PeriodPayment::ConfirmedRate,
    ))
    .await?;
    counter.receive(None).await?;
    let quote = Box::pin(issue_period_rate(
        &mut counter,
        &request,
        period,
        start + 600_000,
    ))
    .await?;
    buyer.receive(None).await?;
    let view = buyer.request(&request).await?;
    assert_eq!(view.label.as_deref(), Some(label.as_str()));
    let shown = view
        .periods
        .iter()
        .find(|p| p.billing_period == period)
        .ok_or("missing period")?;
    assert!(shown
        .details
        .iter()
        .any(|d| d.msg_id == quote && d.app_id == counter.app_id()));
    let (mut buyer, attempt) = tokio::spawn(Box::pin(async move {
        let priced = buyer
            .pay_period(&request, &period, PayOptions::default())
            .await;
        let refusal = priced.outcome.unwrap_err();
        let Some(paykit_sdk::Refusal::PricedByRate { payment }) = refusal.refusal() else {
            panic!("the sibling's signed rate was not offered: {refusal:?}");
        };
        assert_eq!(
            payment.source,
            paykit_sdk::RateSource::PeriodDetails { msg_id: quote }
        );
        let approved = PayOptions::default().with_rate_authorized((**payment).clone());
        let result = buyer.pay_period(&request, &period, approved).await;
        (buyer, result)
    }))
    .await?;
    let PayOutcome::Paid { attempt_id, .. } = attempt.outcome? else {
        return Err("the confirmed period did not settle".into());
    };
    owner.receive(None).await?;
    let sale = owner
        .payee_activity()
        .await?
        .into_iter()
        .find(|sale| sale.request_hash == Some(request.request_hash))
        .ok_or("missing labelled sale")?;
    assert_eq!(sale.label.as_deref(), Some(label.as_str()));
    let paid = sale
        .periods
        .iter()
        .find(|p| p.billing_period == period)
        .ok_or("missing proof")?;
    let receipt = Box::pin(owner.confirm_payment(&paid.record))
        .await?
        .ok_or("missing original receipt")?;
    buyer.receive(None).await?;
    let received = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    let [received] = received.as_slice() else {
        return Err("expected one receipt".into());
    };
    assert_eq!(received.msg_id, receipt);
    assert_eq!(received.attempt_id, attempt_id);
    let record = owner
        .app()
        .shared_record(&paid.record)
        .await?
        .ok_or("missing period record")?;
    let body = ActivityRecord::read(
        record.content.value().ok_or("missing period value")?,
        &owner.identity(),
        ActivitySide::Payee,
    )?;
    let signed = body.receipt.ok_or("missing signed receipt")?;
    assert_eq!(signed.message().from.app_id, owner.app_id());
    assert_eq!(signed.message().msg_id, receipt);
    counter.receive(None).await?;
    let stop = Box::pin(counter.cancel_sent_request(&sale.record, None)).await?;
    buyer.receive(None).await?;
    let ended = buyer.request(&request).await?;
    assert_eq!(ended.status, paykit_sdk::RequestStatus::Cancelled);
    let record = counter
        .app()
        .shared_record(&sale.record)
        .await?
        .ok_or("missing request record")?;
    let body = ActivityRecord::read(
        record.content.value().ok_or("missing request value")?,
        &counter.identity(),
        ActivitySide::Payee,
    )?;
    let signed = body.cancellation.ok_or("missing signed cancellation")?;
    assert_eq!(signed.message().from.app_id, counter.app_id());
    assert_eq!(signed.message().msg_id, stop);
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    Ok(())
}
