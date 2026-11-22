//! Two wallets of different identities enroll and connect: the seller's first request waits
//! for admission, the payer admits the seller, and the waiting request reaches the payer and
//! is paid and receipted (Paykit spec 1, Switch spec 7).

use std::sync::Arc;

use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, MessageDelivery, PayOptions, PayeeStatus, Paykit, PaykitConfig, RequestStatus,
};
use pubky_switch_client::app::AppError;
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::ClientError;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_harness::World;

use super::{
    btc, endpoint, invoice, on_both_backends, paid_and_receipted, Expected, Owner, TestResult,
};

#[tokio::test]
async fn test_connect_admits_the_seller_and_its_waiting_request_is_paid() -> TestResult {
    on_both_backends(admits_the_seller_and_its_waiting_request_is_paid).await
}

/// Whether `wallet`'s allow list holds a conversation entry for `peer`.
async fn admits(wallet: &mut Paykit, peer: &[u8; 32]) -> TestResult<bool> {
    let allowed = Box::pin(wallet.app_mut().allow_list()).await?;
    Ok(allowed
        .conversations
        .iter()
        .any(|entry| &entry.pubky == peer))
}

async fn admits_the_seller_and_its_waiting_request_is_paid(world: &World) -> TestResult {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut payer = Box::pin(Owner::enroll(world, "payer")).await?;
    let mut seller = Box::pin(Owner::enroll(world, "seller")).await?;
    let mut wallet = Box::pin(payer.join(
        world,
        "wallet",
        AppScopes::new().pay().manage_contacts().build()?,
        PaykitConfig::default(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(seller.join(
        world,
        "shop",
        AppScopes::new().request().manage_contacts().build()?,
        PaykitConfig::default(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    assert_eq!(wallet.identity(), payer.identity(world)?);
    assert_eq!(shop.identity(), seller.identity(world)?);
    let (payer_id, seller_id) = (wallet.identity(), shop.identity());
    assert!(!Box::pin(admits(&mut wallet, &seller_id)).await?);

    // The seller's first request is recorded, and waits: the payer hasn't admitted it.
    let draft = invoice("0.001", "first", "lnbc-first")?;
    let sent = Box::pin(shop.send_request(payer_id, draft)).await?;
    let request = sent.request.ok_or("the request was not recorded")?;
    assert!(
        matches!(sent.delivery, MessageDelivery::WaitingForAdmission { .. }),
        "{sent:?}"
    );
    let sales = Box::pin(shop.payee_activity()).await?;
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::Requested);
    let read = Box::pin(wallet.receive(None)).await?;
    assert!(!read.applied.contains(&sent.msg_id), "{read:?}");
    assert!(Box::pin(wallet.requests()).await?.is_empty());

    // The payer's identity takes no connect requests, so the seller can't ask; the payer
    // admits the seller itself.
    let asked = Box::pin(shop.app_mut().ask_to_connect(&payer_id)).await;
    assert!(
        matches!(
            asked,
            Err(AppError::Switch(ClientError::Switch {
                code: ErrorCode::NotAdmitted,
                ..
            }))
        ),
        "{asked:?}"
    );
    Box::pin(wallet.allow_connect(&seller_id)).await?;
    assert!(Box::pin(admits(&mut wallet, &seller_id)).await?);

    // The waiting request now goes and is applied once.
    Box::pin(shop.flush()).await?;
    let delivery = Box::pin(shop.message_delivery(&sent.msg_id)).await?;
    assert!(
        matches!(delivery, Some(MessageDelivery::Accepted { .. })),
        "{delivery:?}"
    );
    let read = Box::pin(wallet.receive(None)).await?;
    assert_eq!(
        read.applied.iter().filter(|id| **id == sent.msg_id).count(),
        1,
        "{read:?}"
    );
    let requests = Box::pin(wallet.requests()).await?;
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Open
    );

    // The payer's proof reaches the seller, and the seller's receipt reaches the payer.
    let outcome = Box::pin(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome?;
    let expected = Expected {
        request_hash: Some(request.request_hash),
        amount: btc("0.001")?,
        endpoint_id: endpoint("btc-lightning-bolt11")?,
    };
    Box::pin(paid_and_receipted(
        &mut wallet,
        &mut shop,
        &rail,
        outcome,
        &expected,
    ))
    .await?;
    Ok(())
}
