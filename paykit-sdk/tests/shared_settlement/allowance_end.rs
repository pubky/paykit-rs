//! Enrolled payer and seller wallets follow the proposal's withdrawal and end rules.
use super::*;
use paykit_lib::allowances::AllowanceTerms;
use paykit_sdk::{AllowanceRole, AllowanceStatus, NoticeKind, Refusal};
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

async fn wallets(
    w: &World,
    payer_store: Arc<MemoryStore>,
    seller_store: Arc<MemoryStore>,
    rail: &Arc<FakeRail>,
) -> TestResult<(Paykit, Paykit)> {
    let mut buyer = Box::pin(setup_identity(w, "buyer", "https://buyer.allowance.test")).await?;
    let mut seller = Box::pin(setup_identity(w, "seller", "https://seller.allowance.test")).await?;
    let mut payer = Box::pin(join(
        w,
        &mut buyer,
        "buyer",
        "wallet",
        AppScopes::new().allowance_execute().build()?,
        payer_store,
        rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        w,
        &mut seller,
        "seller",
        "shop",
        AppScopes::new().allowance().request().build()?,
        seller_store,
        rail,
    ))
    .await?;
    payer.app_mut().allow_connect(&shop.identity()).await?;
    shop.app_mut().allow_connect(&payer.identity()).await?;
    Ok((payer, shop))
}

fn terms() -> TestResult<AllowanceTerms> {
    Ok(AllowanceTerms::from_json(&serde_json::json!({
        "asset": "btc", "per_payment_amount": {"minimum": "0.0001", "maximum": "0.01"},
        "period_limits": [], "lifetime_amount_limit": "0.1", "active_from": null,
        "expires_at": null, "allowed_payment_endpoint_identifiers": null
    }))?)
}

async fn ends(w: &World) -> TestResult {
    let rail = Arc::new(FakeRail::with_clock(w.clock().clone()));
    let store = Arc::new(MemoryStore::new());
    let (mut payer, mut shop) = Box::pin(wallets(
        w,
        store.clone(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let terms = terms()?;
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

#[tokio::test]
async fn test_enrolled_payer_withdraws_its_retired_proposal_after_restore() -> TestResult {
    Box::pin(enrolled_withdrawal(AllowanceRole::Allower)).await
}

#[tokio::test]
async fn test_enrolled_seller_withdraws_its_retired_proposal_after_restore() -> TestResult {
    Box::pin(enrolled_withdrawal(AllowanceRole::Allowee)).await
}

async fn enrolled_withdrawal(role: AllowanceRole) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(withdraw_after_restore(&world, role)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn withdraw_after_restore(w: &World, role: AllowanceRole) -> TestResult {
    let rail = Arc::new(FakeRail::with_clock(w.clock().clone()));
    let payer_store = Arc::new(MemoryStore::new());
    let seller_store = Arc::new(MemoryStore::new());
    let (payer, shop) =
        Box::pin(wallets(w, payer_store.clone(), seller_store.clone(), &rail)).await?;
    let (mut proposer, mut recipient, store) = if role == AllowanceRole::Allower {
        (payer, shop, payer_store)
    } else {
        (shop, payer, seller_store)
    };
    let peer = recipient.identity();
    let offer = if role == AllowanceRole::Allower {
        Box::pin(proposer.propose_executable_allowance(peer, terms()?, UserConfirmed::new()))
            .await?
    } else {
        assert!(!proposer
            .app()
            .cert()
            .holds_scope("paykit_allowance_execute"));
        Box::pin(proposer.propose_allowance(peer, role, terms()?)).await?
    };
    let backup = proposer.backup().await?;
    Box::pin(recipient.receive(None)).await?;
    let incoming = recipient
        .allowances()
        .await?
        .into_iter()
        .find(|v| v.allowance_id == offer.allowance_id)
        .ok_or("missing proposal at recipient")?;
    let rejection = Box::pin(recipient.reject_allowance(&incoming.record)).await?;
    let read = Box::pin(proposer.receive(None)).await?;
    assert!(read
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived {msg_id,..} if msg_id == &rejection)));
    let sent = proposer.app().sent_to(&peer, 0, u64::MAX).await?;
    assert!(!sent.iter().any(|m| m.kind == "paykit.allowance_end"));
    let p = sent
        .iter()
        .find(|m| m.msg_id == offer.msg_id)
        .ok_or("missing signed original proposal")?;
    let original = paykit_lib::records::StoredMessage::new(
        proposer.identity(),
        proposer.app().cert().signed(),
        p.plaintext.clone(),
        None,
        &peer,
    )?;
    drop(proposer);
    let mut restored = Box::pin(Paykit::restore(
        w.app_env(store.clone()),
        Default::default(),
        &backup,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    Box::pin(finish_restored_withdrawal(&mut restored)).await?;
    let sent = restored.app().sent_to(&peer, 0, u64::MAX).await?;
    let mut ends = sent.iter().filter(|m| m.kind == "paykit.allowance_end");
    let end = ends.next().ok_or("restored proposer owes a withdrawal")?;
    assert!(ends.next().is_none());
    let signed = paykit_lib::records::StoredMessage::new(
        restored.identity(),
        restored.app().cert().signed(),
        end.plaintext.clone(),
        None,
        &peer,
    )?;
    assert_eq!(signed.message().refs.as_slice(), [original.hash()]);
    let received = Box::pin(recipient.receive(None)).await?;
    assert!(received
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived{msg_id,..} if msg_id == &end.msg_id)));
    assert!(restored.app().shared_record(&offer.record).await?.is_none());
    assert!(restored
        .app()
        .records_of("allowance_ledgers")
        .await?
        .is_empty());
    if role == AllowanceRole::Allowee {
        let refused = Box::pin(restored.resume_allowance_executor(&offer.record))
            .await
            .unwrap_err();
        assert_eq!(
            refused.refusal(),
            Some(&Refusal::ScopeMissing("paykit_allowance_execute".into()))
        );
        drop(restored);
        let mut again = Box::pin(Paykit::restore(
            w.app_env(store),
            Default::default(),
            &backup,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        Box::pin(finish_restored_withdrawal(&mut again)).await?;
        let original_end = format!("end/{}", end.msg_id);
        let origins = again.app().protocol_scan("paykit", b"end/").await?;
        assert!(origins
            .iter()
            .all(|(key, _)| key == original_end.as_bytes()));
        for remaining in (0..4).rev() {
            let received = Box::pin(recipient.receive(None)).await?;
            assert!(!received.report.events.iter().any(|e| match e {
                AppEvent::MessageReceived { kind, .. } => kind == "paykit.allowance_end",
                AppEvent::Duplicate { .. } => true,
                _ => false,
            }));
            if !received.report.more {
                break;
            }
            assert!(remaining > 0, "recipient still has unread entries");
        }
    }
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn finish_restored_withdrawal(restored: &mut Paykit) -> TestResult {
    for _ in 0..4 {
        let read = Box::pin(restored.receive(None)).await?;
        if read.records_unlisted.is_none() && !restored.app().listing_owed().await? {
            break;
        }
    }
    Box::pin(restored.flush()).await?;
    Ok(())
}
