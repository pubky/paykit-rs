//! The paying wallet receives the seller's end before or after placing the acceptance.
use super::*;
use paykit_lib::allowances::AllowanceTerms;
use paykit_sdk::{AllowanceStatus, NoticeKind};
use pubky_switch_client::app::{AppEvent, UserConfirmed};

#[tokio::test]
async fn test_enrolled_allowance_ends_before_and_after_placing_the_original() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(ends(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn ends(w: &World) -> TestResult {
    let mut buyer = Box::pin(setup_identity(w, "buyer", "https://buyer.allowance.test")).await?;
    let mut seller = Box::pin(setup_identity(w, "seller", "https://seller.allowance.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(w.clock().clone()));
    let store = Arc::new(MemoryStore::new());
    let mut payer = Box::pin(join(
        w,
        &mut buyer,
        "buyer",
        "wallet",
        AppScopes::new().allowance_execute().build()?,
        store.clone(),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        w,
        &mut seller,
        "seller",
        "shop",
        AppScopes::new().allowance().request().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    payer.app_mut().allow_connect(&shop.identity()).await?;
    shop.app_mut().allow_connect(&payer.identity()).await?;
    let terms = AllowanceTerms::from_json(&serde_json::json!({
        "asset": "btc", "per_payment_amount": {"minimum": "0.0001", "maximum": "0.01"},
        "period_limits": [], "lifetime_amount_limit": "0.1", "active_from": null,
        "expires_at": null, "allowed_payment_endpoint_identifiers": null
    }))?;
    let mut offers = Vec::new();
    for placed in [false, true] {
        let offer = Box::pin(payer.propose_executable_allowance(
            shop.identity(),
            terms.clone(),
            UserConfirmed::new(),
        ))
        .await?;
        Box::pin(shop.receive(None)).await?;
        let at_shop = shop
            .allowances()
            .await?
            .into_iter()
            .find(|v| v.allowance_id == offer.allowance_id)
            .ok_or("missing proposal at shop")?;
        let answer = Box::pin(shop.accept_allowance(&at_shop.record)).await?;
        if placed {
            Box::pin(payer.receive(None)).await?;
            assert_eq!(
                payer.allowance(&offer.record).await?.status,
                AllowanceStatus::Accepted
            );
        }
        let end = Box::pin(shop.end_allowance(&at_shop.record)).await?;
        let first = Box::pin(payer.receive(None)).await?;
        for id in std::iter::once(&end).chain((!placed).then_some(&answer)) {
            assert!(
                first
                    .report
                    .events
                    .iter()
                    .any(|e| matches!(e, AppEvent::MessageReceived { msg_id, .. } if msg_id == id)),
                "{first:?}"
            );
        }
        assert_eq!(
            payer.allowance(&offer.record).await?.status,
            AllowanceStatus::Ended
        );
        offers.push((placed, offer.record, answer));
    }
    let app_id = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(Paykit::load(
        w.app_env(store),
        Default::default(),
        app_id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    w.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    Box::pin(payer.receive(None)).await?;
    for (placed, key, answer) in offers {
        let view = payer.allowance(&key).await?;
        assert_eq!(view.status, AllowanceStatus::Ended);
        assert!(!view.consented && view.executor_app_id.is_none());
        assert_eq!(payer.app().shared_record(&key).await?.is_some(), placed);
        assert_eq!(
            payer
                .notices()
                .await?
                .iter()
                .any(|n| n.kind == NoticeKind::MessageGivenUp
                    && n.msg_id.as_deref() == Some(answer.as_str())),
            !placed
        );
    }
    assert_eq!(payer.app().records_of("allowance_ledgers").await?.len(), 1);
    assert!(rail.all_executions().is_empty());
    Ok(())
}
