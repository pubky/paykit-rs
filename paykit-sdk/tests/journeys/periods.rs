//! A recurring request: accepted as a subscription, and its Billing Periods paid by hand or
//! by the app's policy, each proven, credited and receipted (Paykit spec 8.2).

use paykit_lib::bodies::{PeriodDetails, RequestDestination};
use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
use paykit_lib::PaymentReference;
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, PeriodPrice, ReceiptsOf, Refusal,
    RequestDraft, RequestRef, RequestStatus, Stage,
};
use pubky_switch_harness::World;

use super::{btc, checkout, endpoint, on_both_backends, payload, Checkout, TestResult};

const DAY_MS: u64 = 86_400_000;

#[tokio::test]
async fn test_recurring_request_periods_paid_by_hand_and_receipted() -> TestResult {
    on_both_backends(paid_by_hand).await
}

#[tokio::test]
async fn test_recurring_request_periods_paid_by_policy_and_receipted() -> TestResult {
    on_both_backends(paid_by_policy).await
}

/// An hourly request for 0.00025 btc from now, its first period paid to `first`; with
/// `per_period`, each later period is paid to the endpoints of its period details.
fn hourly(
    world: &World,
    first: &str,
    per_period: bool,
) -> TestResult<(RequestDraft, Vec<PeriodRef>)> {
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Hour)?, start, None)?;
    let periods = recurrence
        .periods(2)?
        .into_iter()
        .map(PeriodRef::from)
        .collect();
    let endpoints = [(endpoint("btc-lightning-bolt11")?, payload(first)?)]
        .into_iter()
        .collect();
    let draft = RequestDraft::new(
        btc("0.00025")?,
        PaymentReference::new("hourly-plan")?,
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(start + DAY_MS)
    .with_recurrence(recurrence);
    let draft = if per_period {
        draft.with_per_period_endpoints()
    } else {
        draft
    };
    Ok((draft, periods))
}

/// The seller sends `draft`; the payer accepts the subscription and the seller reads the
/// acceptance. Returns the request and the seller's record of it.
async fn subscribed(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    draft: RequestDraft,
) -> TestResult<(RequestRef, [u8; 32])> {
    let sent = Box::pin(shop.send_request(wallet.identity(), draft)).await?;
    let request = sent.request.ok_or("the request was not sent")?;
    Box::pin(wallet.receive(None)).await?;
    let view = Box::pin(wallet.request(&request)).await?;
    assert!(view.recurring, "{view:?}");
    assert_eq!(view.status, RequestStatus::Open);
    Box::pin(wallet.accept_subscription(&request)).await?;
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Subscribed
    );
    Box::pin(shop.receive(None)).await?;
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].request_hash, Some(request.request_hash));
    assert_eq!(sales[0].status, PayeeStatus::Accepted);
    assert!(sales[0].recurring);
    Ok((request, sales[0].record))
}

/// Move the world's clock to the start of `period`.
fn at_start(world: &World, period: PeriodRef) {
    world.advance_ms(period.starts_at.saturating_sub(world.now_ms()));
}

/// After the payer's `outcome` for `period`: the method was called once, at `published`;
/// the payer's journal holds the period's payment, finished, and its request shows the
/// period paid; the seller holds the proof, verifies and confirms it and shows the period
/// credited; and the payer then holds exactly that receipt. Returns the receipt's `msg_id`.
#[allow(clippy::too_many_arguments)]
async fn period_paid_and_receipted(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    rail: &FakeRail,
    outcome: PayOutcome,
    request: RequestRef,
    sale: [u8; 32],
    period: PeriodRef,
    published: &str,
) -> TestResult<String> {
    let PayOutcome::Paid {
        attempt_id,
        proof_msg_id: Some(_),
        ..
    } = outcome
    else {
        return Err(format!("the period was not paid with a proof: {outcome:?}").into());
    };
    assert_eq!(rail.executions(&attempt_id), 1);
    let selected = rail.selections();
    let (chosen, _) = selected.last().ok_or("the method chose no endpoint")?;
    assert_eq!(chosen.len(), 1, "{chosen:?}");
    assert_eq!(chosen[0].payload.as_str(), published);

    // The payer's view: the period's payment, finished, and the period paid.
    let payments = Box::pin(wallet.payments()).await?;
    let payment = payments
        .iter()
        .find(|payment| payment.attempt_id == attempt_id)
        .ok_or("the payer's journal lacks the payment")?;
    assert_eq!(payment.payee, shop.identity());
    assert_eq!(payment.request_hash, Some(request.request_hash));
    assert_eq!(payment.billing_period, Some(period));
    assert_eq!(payment.amount, btc("0.00025")?);
    assert_eq!(payment.stage, Stage::Finished, "{payment:?}");
    let view = Box::pin(wallet.request(&request)).await?;
    let paid = view
        .periods
        .iter()
        .find(|view| view.billing_period == period)
        .ok_or("the payer's request lacks the period")?;
    assert!(paid.paid, "{view:?}");

    // The seller's view: the period paid, verified, then credited with its receipt.
    Box::pin(shop.receive(None)).await?;
    let held = Box::pin(seller_period(shop, sale, period)).await?;
    assert!(held.paid && !held.credited, "{held:?}");
    assert!(Box::pin(shop.verify_payment(&held.record)).await?.is_some());
    let receipt = Box::pin(shop.confirm_payment(&held.record))
        .await?
        .ok_or("the seller could not confirm the period")?;
    let credited = Box::pin(seller_period(shop, sale, period)).await?;
    assert!(credited.credited, "{credited:?}");
    let issued = credited
        .receipt
        .as_ref()
        .ok_or("the seller shows no receipt")?;
    assert_eq!(issued.msg_id, receipt);
    assert_eq!(issued.attempt_id, attempt_id);

    // The payer's view again: the seller's receipt of this period.
    Box::pin(wallet.receive(None)).await?;
    let receipts = Box::pin(wallet.receipts(ReceiptsOf::Period(request, period))).await?;
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].msg_id, receipt);
    assert_eq!(receipts[0].attempt_id, attempt_id);
    assert_eq!(receipts[0].billing_period, Some(period));
    Ok(receipt)
}

/// The seller's view of `period` of its subscription record `sale`.
async fn seller_period(
    shop: &Paykit,
    sale: [u8; 32],
    period: PeriodRef,
) -> TestResult<paykit_sdk::PeriodView> {
    let sales = Box::pin(shop.payee_activity()).await?;
    let subscription = sales
        .iter()
        .find(|activity| activity.record == sale)
        .ok_or("the seller lost the subscription")?;
    Ok(subscription
        .periods
        .iter()
        .find(|view| view.billing_period == period)
        .ok_or_else(|| format!("the seller has no record of the period: {subscription:?}"))?
        .clone())
}

async fn paid_by_hand(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let (draft, periods) = hourly(world, "lnbc-first-period", true)?;
    let (request, sale) = Box::pin(subscribed(&mut wallet, &mut shop, draft)).await?;

    // The first period is paid by hand, to the request's endpoint, once.
    let first = Box::pin(wallet.pay_period(&request, &periods[0], PayOptions::default()))
        .await
        .outcome?;
    let again = Box::pin(wallet.pay_period(&request, &periods[0], PayOptions::default()))
        .await
        .outcome;
    let refused = again.err().ok_or("a paid period was paid again")?;
    assert_eq!(refused.refusal(), Some(&Refusal::AlreadyPaid));
    Box::pin(period_paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        first,
        request,
        sale,
        periods[0],
        "lnbc-first-period",
    ))
    .await?;

    // At its start, the second period's details name its own invoice, and it is paid there.
    at_start(world, periods[1]);
    let details = PeriodDetails {
        billing_period: periods[1],
        payment_endpoints: Some(
            [(
                endpoint("btc-lightning-bolt11")?,
                payload("lnbc-second-period")?,
            )]
            .into_iter()
            .collect(),
        ),
        rates: None,
        expires_at: periods[1].ends_at,
    };
    let sent = Box::pin(shop.send_period_details(&sale, details)).await?;
    Box::pin(wallet.receive(None)).await?;
    let view = Box::pin(wallet.request(&request)).await?;
    let second = view
        .periods
        .iter()
        .find(|view| view.billing_period == periods[1])
        .ok_or("the payer did not get the second period's details")?;
    assert_eq!(second.details.len(), 1, "{second:?}");
    assert_eq!(second.details[0].msg_id, sent.msg_id);
    assert!(!second.paid);
    let outcome = Box::pin(wallet.pay_period(&request, &periods[1], PayOptions::default()))
        .await
        .outcome?;
    Box::pin(period_paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        request,
        sale,
        periods[1],
        "lnbc-second-period",
    ))
    .await?;
    assert_eq!(rail.all_executions().len(), 2);
    assert_eq!(Box::pin(wallet.payments()).await?.len(), 2);
    Ok(())
}

async fn paid_by_policy(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let (draft, periods) = hourly(world, "lnbc-plan", false)?;
    let (request, sale) = Box::pin(subscribed(&mut wallet, &mut shop, draft)).await?;

    // The app's policy pays each period once it starts, at the requested amount, and not
    // before.
    let early = Box::pin(wallet.pay_period_automatically(
        &request,
        &periods[1],
        PeriodPrice::RequestedAmount,
    ))
    .await
    .outcome;
    let refused = early
        .err()
        .ok_or("a period was paid by policy before its start")?;
    assert_eq!(
        refused.refusal(),
        Some(&Refusal::PeriodNotStarted),
        "{refused:?}"
    );
    assert!(Box::pin(wallet.payments()).await?.is_empty());
    assert!(rail.all_executions().is_empty());
    for &period in &periods {
        at_start(world, period);
        let outcome = Box::pin(wallet.pay_period_automatically(
            &request,
            &period,
            PeriodPrice::RequestedAmount,
        ))
        .await
        .outcome?;
        Box::pin(period_paid_and_receipted(
            &mut wallet,
            &mut shop,
            &rail,
            outcome,
            request,
            sale,
            period,
            "lnbc-plan",
        ))
        .await?;
    }
    assert_eq!(rail.all_executions().len(), 2);
    assert_eq!(Box::pin(wallet.payments()).await?.len(), 2);
    Ok(())
}
