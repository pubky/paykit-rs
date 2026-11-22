//! A one-time request: sent, paid, proven and receipted (Paykit spec 8.1), and seen by a
//! second wallet of the payer's identity (Paykit spec 8.5).

use std::sync::Arc;

use paykit_sdk::{
    AppScopes, PayOptions, PayeeStatus, Paykit, PaykitConfig, ReceiptsOf, Refusal, RequestRef,
    RequestStatus,
};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_harness::World;

use super::{
    btc, checkout, endpoint, invoice, on_both_backends, paid_and_receipted, sale_of, Checkout,
    Expected, TestResult,
};

#[tokio::test]
async fn test_one_time_request_paid_proven_and_receipted() -> TestResult {
    on_both_backends(paid_proven_and_receipted).await
}

#[tokio::test]
async fn test_one_time_request_seen_paid_and_receipted_by_a_second_wallet() -> TestResult {
    on_both_backends(second_wallet_sees_the_payment).await
}

/// The seller sends a request; the seller shows it requested, and the payer's wallet
/// receives it and shows it open.
async fn requested(wallet: &mut Paykit, shop: &mut Paykit) -> TestResult<RequestRef> {
    let sent = Box::pin(shop.send_request(
        wallet.identity(),
        invoice("0.00025", "order-1", "lnbc-order-1")?,
    ))
    .await?;
    let request = sent.request.ok_or("the request was not sent")?;
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].request_hash, Some(request.request_hash));
    assert_eq!(sales[0].payer, wallet.identity());
    assert_eq!(sales[0].status, PayeeStatus::Requested);
    let read = Box::pin(wallet.receive(None)).await?;
    assert!(read.applied.contains(&sent.msg_id), "{read:?}");
    let view = Box::pin(wallet.request(&request)).await?;
    assert_eq!(view.status, RequestStatus::Open);
    assert_eq!(view.amount, btc("0.00025")?);
    assert!(Box::pin(wallet.payments()).await?.is_empty());
    Ok(request)
}

fn expected(request: &RequestRef) -> TestResult<Expected> {
    Ok(Expected {
        request_hash: Some(request.request_hash),
        amount: btc("0.00025")?,
        endpoint_id: endpoint("btc-lightning-bolt11")?,
    })
}

/// Paying `request` again from `wallet` is refused, and its journal holds no new payment.
async fn refused_as_paid(wallet: &mut Paykit, request: &RequestRef, paid: usize) -> TestResult {
    let again = Box::pin(wallet.pay_request(request, PayOptions::default()))
        .await
        .outcome;
    let refused = again.err().ok_or("a paid request was paid again")?;
    assert_eq!(refused.refusal(), Some(&Refusal::AlreadyPaid));
    assert_eq!(Box::pin(wallet.payments()).await?.len(), paid);
    Ok(())
}

async fn paid_proven_and_receipted(world: &World) -> TestResult {
    let Checkout {
        mut wallet,
        mut shop,
        rail,
        ..
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let request = Box::pin(requested(&mut wallet, &mut shop)).await?;
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Paid
    );
    Box::pin(refused_as_paid(&mut wallet, &request, 1)).await?;
    let receipt = Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected(&request)?,
    ))
    .await?;
    let of_request = Box::pin(wallet.receipts(ReceiptsOf::Request(request))).await?;
    assert_eq!(of_request.len(), 1, "{of_request:?}");
    assert_eq!(of_request[0].msg_id, receipt);
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Paid
    );
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}

async fn second_wallet_sees_the_payment(world: &World) -> TestResult {
    let Checkout {
        mut payer,
        mut wallet,
        mut shop,
        rail,
    } = Box::pin(checkout(world, PaykitConfig::default())).await?;
    let mut phone = Box::pin(payer.join(
        world,
        "phone",
        AppScopes::new().pay().manage_contacts().build()?,
        PaykitConfig::default(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    assert_eq!(phone.identity(), payer.identity(world)?);
    assert_ne!(phone.app_id(), wallet.app_id());
    let request = Box::pin(requested(&mut wallet, &mut shop)).await?;
    Box::pin(phone.receive(None)).await?;
    assert_eq!(
        Box::pin(phone.request(&request)).await?.status,
        RequestStatus::Open
    );

    // The wallet pays; the phone sees the request paid and doesn't pay it a second time.
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    Box::pin(phone.receive(None)).await?;
    assert_eq!(
        Box::pin(phone.request(&request)).await?.status,
        RequestStatus::Paid
    );
    Box::pin(refused_as_paid(&mut phone, &request, 0)).await?;

    // The seller receipts the wallet's payment; the receipt reaches the phone too.
    let receipt = Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected(&request)?,
    ))
    .await?;
    Box::pin(phone.receive(None)).await?;
    let on_phone = Box::pin(phone.receipts(ReceiptsOf::Request(request))).await?;
    assert_eq!(on_phone.len(), 1, "{on_phone:?}");
    assert_eq!(on_phone[0].msg_id, receipt);
    assert!(Box::pin(phone.payments()).await?.is_empty());
    assert_eq!(Box::pin(shop.payee_activity()).await?.len(), 1);
    let sale = Box::pin(sale_of(&shop, on_phone[0].attempt_id)).await?;
    assert_eq!(sale.status, PayeeStatus::Receipted);
    assert_eq!(rail.all_executions().len(), 1);
    Ok(())
}
