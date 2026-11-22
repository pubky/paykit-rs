//! Paying a contact without a request, at the endpoint of the Private Payment List it shared,
//! and that list updated and withdrawn (Paykit spec 8.4).

use std::collections::BTreeMap;

use paykit_lib::identifiers::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{Paykit, PaykitConfig, Refusal, UnrequestedPayment};
use pubky_switch_harness::World;

use super::{
    btc, checkout, endpoint, on_both_backends, paid_and_receipted, payload, Checkout, Expected,
    TestResult,
};

#[tokio::test]
async fn test_private_list_paid_without_a_request_credited_and_receipted() -> TestResult {
    on_both_backends(paid_without_a_request).await
}

#[tokio::test]
async fn test_private_list_updated_then_withdrawn() -> TestResult {
    on_both_backends(updated_then_withdrawn).await
}

fn offer(text: &str) -> TestResult<BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
    Ok([(endpoint("btc-lightning-bolt12")?, payload(text)?)]
        .into_iter()
        .collect())
}

fn expected(value: &str) -> TestResult<Expected> {
    Ok(Expected::new(
        None,
        btc(value)?,
        endpoint("btc-lightning-bolt12")?,
    ))
}

/// The seller sets `text` as its list's endpoints and chooses the payer as a contact to
/// publish the list to; the payer's wallet receives it.
async fn shared(wallet: &mut Paykit, shop: &mut Paykit, text: &str) -> TestResult {
    Box::pin(shop.set_list_endpoints(offer(text)?)).await?;
    Box::pin(shop.publish_list_to(wallet.identity())).await?;
    Box::pin(wallet.receive(None)).await?;
    Ok(())
}

/// The seller changes its list's endpoints to `text`, and only that: the changed list is
/// owed to each contact already chosen. The payer's wallet receives it.
async fn updated(wallet: &mut Paykit, shop: &mut Paykit, text: &str) -> TestResult {
    Box::pin(shop.set_list_endpoints(offer(text)?)).await?;
    Box::pin(wallet.receive(None)).await?;
    Ok(())
}

/// The wallet pays the seller `value` without a request, from the list it holds; the method
/// is handed the payload `published`, and the payment is credited and receipted.
async fn pay_from_list(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    rail: &FakeRail,
    value: &str,
    published: &str,
) -> TestResult<String> {
    let payment = UnrequestedPayment::new(shop.identity(), btc(value)?);
    let before = rail.selections().len();
    let outcome = Box::pin(wallet.pay_without_request(payment))
        .await
        .outcome?;
    let selections = rail.selections();
    assert_eq!(selections.len(), before + 1, "{selections:?}");
    let (candidates, _) = selections
        .last()
        .ok_or("the method was handed no endpoint")?;
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0].payload.as_str(), published);
    Box::pin(paid_and_receipted(
        wallet,
        shop,
        rail,
        outcome,
        &expected(value)?,
    ))
    .await
}

/// A payment without a request to the seller is refused with `refusal`, and nothing is paid.
async fn refused(
    wallet: &mut Paykit,
    rail: &FakeRail,
    seller: [u8; 32],
    refusal: impl Fn(&Refusal) -> bool,
) -> TestResult {
    let before = rail.all_executions().len();
    let payments = Box::pin(wallet.payments()).await?.len();
    let payment = UnrequestedPayment::new(seller, btc("0.001")?);
    let outcome = Box::pin(wallet.pay_without_request(payment)).await.outcome;
    let error = outcome
        .err()
        .ok_or("a payment with no list to pay was made")?;
    assert!(error.refusal().is_some_and(&refusal), "{error:?}");
    assert_eq!(rail.all_executions().len(), before);
    assert_eq!(Box::pin(wallet.payments()).await?.len(), payments);
    Ok(())
}

async fn paid_without_a_request(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    Box::pin(shared(&mut wallet, &mut shop, "lno1shop")).await?;
    // A reusable endpoint: the payer pays it twice, and each payment is its own.
    let first = Box::pin(pay_from_list(
        &mut wallet,
        &mut shop,
        &rail,
        "0.001",
        "lno1shop",
    ))
    .await?;
    let second = Box::pin(pay_from_list(
        &mut wallet,
        &mut shop,
        &rail,
        "0.002",
        "lno1shop",
    ))
    .await?;
    assert_ne!(first, second);
    assert_eq!(Box::pin(wallet.payments()).await?.len(), 2);
    assert_eq!(Box::pin(shop.payee_activity()).await?.len(), 2);
    assert_eq!(rail.all_executions().len(), 2);
    Ok(())
}

async fn updated_then_withdrawn(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let seller = shop.identity();

    // Before any list, the payment waits for one.
    Box::pin(refused(&mut wallet, &rail, seller, |refusal| {
        refusal == &Refusal::WaitingForList
    }))
    .await?;

    // The seller shares a list, then updates it; each time the payer pays the current one.
    Box::pin(shared(&mut wallet, &mut shop, "lno1first")).await?;
    Box::pin(pay_from_list(
        &mut wallet,
        &mut shop,
        &rail,
        "0.001",
        "lno1first",
    ))
    .await?;
    Box::pin(updated(&mut wallet, &mut shop, "lno1second")).await?;
    Box::pin(pay_from_list(
        &mut wallet,
        &mut shop,
        &rail,
        "0.001",
        "lno1second",
    ))
    .await?;

    // The seller withdraws the list; the payer has nothing left to pay at.
    Box::pin(shop.withdraw_list_from(wallet.identity())).await?;
    Box::pin(wallet.receive(None)).await?;
    Box::pin(refused(&mut wallet, &rail, seller, |refusal| {
        matches!(refusal, Refusal::NoDestination(_))
    }))
    .await?;
    assert_eq!(Box::pin(shop.payee_activity()).await?.len(), 2);
    Ok(())
}
