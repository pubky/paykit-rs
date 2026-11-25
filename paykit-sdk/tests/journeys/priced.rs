//! A one-time request priced in another asset by a fixed conversion rate: shown to the
//! payer, paid only at the amount the user confirmed, and credited at it (Paykit spec 7.2
//! step 1, 8.1).

use paykit_lib::bodies::{Conversion, RequestDestination};
use paykit_lib::{Asset, Decimal, PaymentAmount, PaymentReference, Rate};
use paykit_sdk::{
    PayOptions, PaykitConfig, PricedPayment, RateSource, Refusal, RequestDraft, RequestStatus,
};
use pubky_dispatch_harness::World;

use super::{
    btc, checkout, endpoint, on_both_backends, paid_and_receipted, payload, Checkout, Expected,
    TestResult,
};

#[tokio::test]
async fn test_priced_request_paid_at_the_confirmed_rate_and_receipted() -> TestResult {
    on_both_backends(paid_at_the_confirmed_rate).await
}

fn usd(value: &str) -> TestResult<PaymentAmount> {
    Ok(PaymentAmount {
        asset: Asset::new("usd")?,
        value: Decimal::new(value)?,
    })
}

async fn paid_at_the_confirmed_rate(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;

    // 10 usd to a Lightning invoice, at 0.0000123456 btc per usd: 0.000123456 btc, which
    // the method's smallest unit rounds up to 0.00012346.
    let rate = Rate::new(Asset::new("btc")?, Decimal::new("0.0000123456")?)?;
    let endpoints = [(
        endpoint("btc-lightning-bolt11")?,
        payload("lnbc-order-usd")?,
    )]
    .into_iter()
    .collect();
    let draft = RequestDraft::new(
        usd("10")?,
        PaymentReference::new("order-usd")?,
        RequestDestination::Endpoints(endpoints),
    )
    .with_conversion(Conversion::Fixed(vec![rate]));
    let sent = Box::pin(shop.send_request(wallet.identity(), draft)).await?;
    let request = sent.request.ok_or("the request was not sent")?;
    Box::pin(wallet.receive(None)).await?;
    let view = Box::pin(wallet.request(&request)).await?;
    assert_eq!(view.status, RequestStatus::Open);
    assert_eq!(view.amount, usd("10")?);

    // Without the user's confirmation of the computed amount, nothing is paid; the refusal
    // shows what the user is asked to confirm.
    let quoted_from = rail.selections().len();
    let unconfirmed = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome;
    let refused = unconfirmed
        .err()
        .ok_or("a priced request was paid unconfirmed")?;
    let Some(Refusal::PricedByRate { payment }) = refused.refusal() else {
        return Err(format!("not refused as priced: {refused:?}").into());
    };
    let shown: PricedPayment = (**payment).clone();
    // What the user's authorization binds: the payee and its signed request, the app and
    // endpoint the payment goes to, the signed rate and its source, and the amount.
    assert_eq!(shown.request, request);
    assert_eq!(shown.billing_period, None);
    assert_eq!(shown.app_id, shop.app_id());
    assert_eq!(shown.endpoint_id, endpoint("btc-lightning-bolt11")?);
    assert_eq!(shown.requested, usd("10")?);
    assert_eq!(shown.source, RateSource::Request);
    assert_eq!(shown.rate_source_hash, request.request_hash);
    assert_eq!(shown.rate.asset(), &Asset::new("btc")?);
    assert_eq!(shown.rate.value().as_str(), "0.0000123456");
    assert_eq!(shown.amount, btc("0.00012346")?);
    assert!(rail.all_executions().is_empty());
    assert!(Box::pin(wallet.payments()).await?.is_empty());
    // Quoting the computed amount asked the method once.
    assert_eq!(rail.selections().len(), quoted_from + 1);

    // Confirmed, it is paid once at that amount, and credited and receipted.
    let expected = Expected::new(
        &rail,
        Some(request.request_hash),
        usd("10")?,
        endpoint("btc-lightning-bolt11")?,
    )
    .at_rate("0.0000123456", btc("0.00012346")?);
    let options = PayOptions::default().with_rate_authorized(shown);
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
    // The seller checked the proof for exactly the amount the rate computed.
    let checks = rail.proof_checks();
    let check = checks.last().ok_or("the seller checked no proof")?;
    assert_eq!(check.amount, btc("0.00012346")?);
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Paid
    );
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}
