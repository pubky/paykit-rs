//! A public Payment Endpoint: published in the shop's app record, read and paid by a payer,
//! credited and receipted, then removed (Paykit spec 4).

use paykit_sdk::rail::FakeRail;
use paykit_sdk::{PayOptions, Paykit, PaykitConfig, PaymentMode, Refusal, UnrequestedPayment};
use pubky_switch_client::app::{AppConfig, UserConfirmed};
use pubky_switch_harness::World;

use super::{
    btc, checkout, endpoint, on_both_backends, paid_and_receipted, payload, Checkout, Expected,
    TestResult,
};

#[tokio::test]
async fn test_public_endpoint_published_paid_receipted_and_removed() -> TestResult {
    on_both_backends(published_paid_receipted_and_removed).await
}

/// A payment to the seller's public endpoints, which the user confirmed as public.
fn public_payment(seller: [u8; 32]) -> TestResult<UnrequestedPayment> {
    Ok(UnrequestedPayment::new(seller, btc("0.001")?)
        .with_options(PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new()))))
}

/// The payer finds no public endpoint of the seller's; a public payment is refused for want
/// of a destination, and nothing is paid.
async fn nothing_to_pay(wallet: &mut Paykit, rail: &FakeRail, seller: [u8; 32]) -> TestResult {
    let before = rail.all_executions().len();
    let payments = Box::pin(wallet.payments()).await?.len();
    let listed = Box::pin(wallet.public_endpoints(&seller, None)).await?;
    assert!(listed.endpoints.is_empty(), "{listed:?}");
    assert!(listed.failures.is_empty(), "{listed:?}");
    let outcome = Box::pin(wallet.pay_without_request(public_payment(seller)?))
        .await
        .outcome;
    let refused = outcome
        .err()
        .ok_or("paid a seller with no public endpoint")?;
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert_eq!(rail.all_executions().len(), before);
    assert_eq!(Box::pin(wallet.payments()).await?.len(), payments);
    Ok(())
}

async fn published_paid_receipted_and_removed(world: &World) -> TestResult {
    let publishing = PaykitConfig::default().with_app(AppConfig {
        publishes_record: true,
        ..AppConfig::default()
    });
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, publishing)).await?;
    let seller = shop.identity();
    Box::pin(nothing_to_pay(&mut wallet, &rail, seller)).await?;

    // The shop publishes a public endpoint; the payer reads it from the shop's record.
    let tips = endpoint("btc-lightning-bolt12")?;
    Box::pin(shop.set_public_endpoint(tips.clone(), payload("lno1tips")?)).await?;
    let own = shop.own_public_endpoints()?;
    assert_eq!(own.get(&tips), Some(&payload("lno1tips")?), "{own:?}");
    let listed = Box::pin(wallet.public_endpoints(&seller, None)).await?;
    assert!(listed.failures.is_empty(), "{listed:?}");
    assert_eq!(listed.endpoints.len(), 1, "{listed:?}");
    assert_eq!(listed.endpoints[0].endpoint_id, tips);
    assert_eq!(listed.endpoints[0].payload, payload("lno1tips")?);
    assert_eq!(listed.endpoints[0].app_id, shop.app_id());

    // The payer pays it once; the seller credits and receipts the payment.
    let outcome = Box::pin(wallet.pay_without_request(public_payment(seller)?))
        .await
        .outcome?;
    let expected = Expected::new(None, btc("0.001")?, tips.clone());
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected,
    ))
    .await?;

    // The shop removes the endpoint; the payer no longer finds it, and pays nothing.
    Box::pin(shop.remove_public_endpoint(&tips)).await?;
    assert!(shop.own_public_endpoints()?.is_empty());
    Box::pin(nothing_to_pay(&mut wallet, &rail, seller)).await?;
    assert_eq!(Box::pin(shop.payee_activity()).await?.len(), 1);
    Ok(())
}
