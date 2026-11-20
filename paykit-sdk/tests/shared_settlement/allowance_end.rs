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
    payer_scopes: RequestedScopes,
    rail: &Arc<FakeRail>,
) -> TestResult<(Signer, Paykit, Paykit)> {
    let mut buyer = Box::pin(setup_identity(w, "buyer", "https://buyer.allowance.test")).await?;
    let mut seller = Box::pin(setup_identity(w, "seller", "https://seller.allowance.test")).await?;
    let mut payer = Box::pin(join(
        w,
        &mut buyer,
        "buyer",
        "wallet",
        payer_scopes,
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
    Ok((buyer, payer, shop))
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
    let (_, mut payer, mut shop) = Box::pin(wallets(
        w,
        store.clone(),
        Arc::new(MemoryStore::new()),
        AppScopes::new().allowance_execute().build()?,
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
async fn test_enrolled_payer_withdraws_a_recovered_proposal() -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::Original)).await
}

#[tokio::test]
async fn test_enrolled_payer_holds_a_missing_proposal_after_loss() -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::Missing)).await
}

#[tokio::test]
async fn test_enrolled_payer_retains_a_new_pending_lifetime_after_loss() -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::Recreated)).await
}

#[tokio::test]
async fn test_enrolled_payer_recovers_the_unanswered_original_create_and_withdrawal() -> TestResult
{
    Box::pin(terminal_after_loss(RecoveredProposal::Interrupted)).await
}

#[tokio::test]
async fn test_enrolled_payer_keeps_the_recovered_proposal_while_the_seller_is_denied() -> TestResult
{
    Box::pin(terminal_after_loss(RecoveredProposal::Denied)).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecoveredProposal {
    Original,
    Missing,
    Recreated,
    Interrupted,
    Denied,
}

async fn terminal_after_loss(case: RecoveredProposal) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(withdraw_recovered_proposal(&world, case)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn withdraw_recovered_proposal(w: &World, case: RecoveredProposal) -> TestResult {
    use pubky_switch_client::app::{Collection, RecordTarget};
    use pubky_switch_core::ops::Condition;

    let rail = Arc::new(FakeRail::with_clock(w.clock().clone()));
    let payer_store = Arc::new(MemoryStore::new());
    let payer_scopes = AppScopes::new().allowance_execute();
    let payer_scopes = if case == RecoveredProposal::Denied {
        payer_scopes.manage_contacts()
    } else {
        payer_scopes
    };
    let (mut signer, mut payer, mut shop) = Box::pin(wallets(
        w,
        payer_store.clone(),
        Arc::new(MemoryStore::new()),
        payer_scopes.build()?,
        &rail,
    ))
    .await?;
    let peer = shop.identity();
    let before_proposal = payer.backup().await?;
    let original_generation = payer.app().own_identity().generation();
    if case == RecoveredProposal::Interrupted {
        w.lose_answer("POST", "/ops");
    }
    let offer =
        Box::pin(payer.propose_executable_allowance(peer, terms()?, UserConfirmed::new())).await?;
    if case == RecoveredProposal::Interrupted {
        assert!(matches!(
            offer.delivery,
            paykit_sdk::MessageDelivery::Kept { .. }
        ));
        let id = payer.app_id();
        drop(payer);
        payer = Box::pin(Paykit::load(
            w.app_env(payer_store.clone()),
            Default::default(),
            id,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    }
    Box::pin(shop.receive(None)).await?;
    let mirror = shop
        .allowances()
        .await?
        .into_iter()
        .find(|view| view.allowance_id == offer.allowance_id)
        .ok_or("missing seller proposal")?
        .record;
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.policy(None)),
        &natural_key::allowance(&peer, &offer.allowance_id),
    );
    let pending_key = payer.app().record_key(&target)?;
    let (original, created_at, original_op) = payer
        .app_mut()
        .read_record_application(&target)
        .await?
        .ok_or("missing original pending")?;
    assert!(original.authenticated && original_op.proof.is_some());
    assert_eq!(created_at.generation, original_generation);
    assert_eq!(original.writer, payer.app_id());
    let next = "https://replacement.buyer.allowance.test";
    w.lose_switch("https://buyer.allowance.test");
    w.add_switch(next, &[&w.identity("buyer")?]).await?;
    Box::pin(signer.declare_provider_loss(
        next,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    Box::pin(signer.end_staged_admission()).await?;
    if case == RecoveredProposal::Missing {
        drop(payer);
        payer = Box::pin(Paykit::restore(
            w.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            &before_proposal,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        // The restored wallet first reads with its original enrollment.
        Box::pin(payer.receive(None)).await?;
    }
    payer.app_mut().refresh_identity().await?;
    payer.app_mut().follow_provider().await?;
    Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    assert_eq!(
        payer.app().own_identity().body().recovery_from,
        Some(original_generation)
    );
    payer.app_mut().invite(&peer).await?;
    shop.app_mut().identity_of(&payer.identity(), true).await?;
    let selection = if case != RecoveredProposal::Missing {
        let restored = payer.app_mut().read_record_fresh(&target).await?;
        let pubky_switch_client::app::RecordRead::Fresh(live) = restored else {
            return Err("replacement did not return the live pending proposal".into());
        };
        assert_eq!(live.content, original.content);
        let selection = payer.app().record_condition(&pending_key).await?;
        assert!(
            matches!(&selection, Condition::RecordCurrent { key, revision, deleted: false }
            if *key == pending_key && revision.generation == payer.app().own_identity().generation()
                && revision.item_hash == created_at.item_hash)
        );
        let (_, installed, member) = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .expect("installed proposal");
        assert_eq!(member, original_op);
        assert_eq!(installed.item_hash, created_at.item_hash);
        assert!(payer
            .app()
            .own_identity()
            .service_key_of(original_generation)
            .is_none());
        Some(selection)
    } else {
        None
    };
    let answer = Box::pin(shop.accept_allowance(&mirror)).await?;
    assert_eq!(
        shop.allowance(&mirror).await?.status,
        AllowanceStatus::Accepted
    );
    if case == RecoveredProposal::Recreated {
        let pending = original.content.value().ok_or("missing proposal value")?;
        payer
            .app_mut()
            .delete_record(&target, vec![selection.clone().unwrap()])
            .await?;
        let condition = payer.app().record_condition(&pending_key).await?;
        let later = payer
            .app_mut()
            .put_record(&target, pending, vec![condition])
            .await?;
        assert_ne!(later.version, original.version);
        Box::pin(payer.receive(None)).await?;
        Box::pin(payer.flush()).await?;
        let (retained, _, _) = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .ok_or("missing later lifetime")?;
        assert_eq!(retained.version, later.version);
        assert_eq!(retained.content, original.content);
        assert!(payer
            .notices()
            .await?
            .iter()
            .any(|n| n.kind == NoticeKind::AllowanceAcceptanceBeforeLoss
                && n.msg_id.as_deref() == Some(answer.as_str())));
    }
    if case == RecoveredProposal::Denied {
        Box::pin(payer.block(&peer)).await?;
        Box::pin(payer.receive(None)).await?;
        Box::pin(payer.flush()).await?;
        let (_, revision, _) = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .ok_or("missing blocked proposal")?;
        assert_eq!(revision.item_hash, created_at.item_hash);
    }
    if matches!(
        case,
        RecoveredProposal::Denied | RecoveredProposal::Recreated
    ) {
        assert!(!payer
            .app()
            .sent_to(&peer, 0, u64::MAX)
            .await?
            .iter()
            .any(|m| m.kind == "paykit.allowance_end"));
        assert_eq!(
            shop.allowance(&mirror).await?.status,
            AllowanceStatus::Accepted
        );
        assert!(payer.app().shared_record(&offer.record).await?.is_none());
        assert!(payer
            .app()
            .records_of("allowance_ledgers")
            .await?
            .is_empty());
        assert!(rail.all_executions().is_empty());
        return Ok(());
    }
    if case == RecoveredProposal::Interrupted {
        w.lose_answer("POST", "/ops");
    }
    let read = Box::pin(payer.receive(None)).await?;
    assert!(read
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived {msg_id,..} if msg_id == &answer)));
    if case == RecoveredProposal::Missing {
        assert!(payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .is_none());
        assert!(matches!(
            payer
                .end_allowance(&offer.record)
                .await
                .unwrap_err()
                .refusal(),
            Some(Refusal::NotFound)
        ));
        Box::pin(payer.flush()).await?;
        assert!(!payer
            .app()
            .sent_to(&peer, 0, u64::MAX)
            .await?
            .iter()
            .any(|m| m.kind == "paykit.allowance_end"));
        Box::pin(shop.receive(None)).await?;
        assert_eq!(
            shop.allowance(&mirror).await?.status,
            AllowanceStatus::Accepted
        );
        assert!(payer.app().shared_record(&offer.record).await?.is_none());
        assert!(payer
            .app()
            .records_of("allowance_ledgers")
            .await?
            .is_empty());
        assert!(rail.all_executions().is_empty());
        return Ok(());
    }
    assert!(payer
        .notices()
        .await?
        .iter()
        .any(|n| n.kind == NoticeKind::AllowanceAcceptanceBeforeLoss
            && n.msg_id.as_deref() == Some(&answer)));
    if case == RecoveredProposal::Interrupted {
        let kept = payer.app().protocol_scan("paykit", b"ap/").await?;
        assert_eq!(
            kept.len(),
            1,
            "the service answer was lost before the wallet could release its signed withdrawal"
        );
        let id = payer.app_id();
        drop(payer);
        payer = Box::pin(Paykit::load(
            w.app_env(payer_store.clone()),
            Default::default(),
            id,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    }
    assert!(payer
        .resume_allowance_executor(&offer.record)
        .await
        .is_err());
    let selection = selection.unwrap();
    Box::pin(payer.flush()).await?;
    let sent = payer.app().sent_to(&peer, 0, u64::MAX).await?;
    let end = sent
        .iter()
        .find(|item| item.kind == "paykit.allowance_end")
        .expect("receiving the fenced original acceptance withdraws the exact live proposal")
        .msg_id
        .clone();
    let (deleted, at, deletion) = payer
        .app_mut()
        .read_record_application(&target)
        .await?
        .ok_or("missing applied pending deletion")?;
    assert!(deleted.authenticated && deletion.proof.is_some() && deleted.content.value().is_none());
    assert_eq!(at.generation, payer.app().own_identity().generation());
    let op = pubky_switch_core::ops::SignedOp::verify_with_cert(
        &deletion.op,
        payer.app().cert(),
        &payer.identity(),
    )?;
    assert!(op.body().conditions.contains(&selection));
    assert!(
        matches!(&op.body().kind, pubky_switch_core::ops::OpKind::RecordPut(put)
        if put.key == pending_key && put.expected_version == original.version)
    );
    let read = Box::pin(shop.receive(None)).await?;
    assert!(
        read.report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageReceived {msg_id,..} if msg_id == &end)),
        "{read:?}"
    );
    assert_eq!(
        shop.allowance(&mirror).await?.status,
        AllowanceStatus::Ended
    );
    let held = shop
        .app()
        .shared_record(&mirror)
        .await?
        .ok_or("missing seller mirror")?;
    let stored = paykit_lib::records::AllowanceRecord::read(
        held.content.value().ok_or("unreadable seller mirror")?,
        &peer,
    )?;
    let response = stored.response.as_ref().ok_or("missing acceptance")?;
    let ended = stored.end.as_ref().ok_or("missing signed end")?;
    assert_eq!(response.message().msg_id, answer);
    assert_eq!(ended.sender(), peer);
    assert!(stored.msg_ids.contains(&end));
    let sent = payer.app().sent_to(&peer, 0, u64::MAX).await?;
    let mut withdrawals = sent.iter().filter(|m| m.kind == "paykit.allowance_end");
    let withdrawal = withdrawals.next().ok_or("missing delivered withdrawal")?;
    assert!(withdrawals.next().is_none());
    assert_eq!(withdrawal.msg_id, end);
    let signed = paykit_lib::records::StoredMessage::new(
        payer.identity(),
        payer.app().cert().signed(),
        withdrawal.plaintext.clone(),
        None,
        &peer,
    )?;
    assert_eq!(signed.message().refs.as_slice(), [stored.proposal.hash()]);
    let read = Box::pin(payer.receive(None)).await?;
    assert!(read.report.events.iter().any(
        |event| matches!(event, AppEvent::MessageReceived {msg_id,..}
        if msg_id == &ended.message().msg_id)
    ));
    assert!(matches!(
        payer.allowance(&offer.record).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    assert_eq!(
        ended.message().refs,
        [stored.proposal.hash(), response.hash()]
    );
    assert!(payer
        .app()
        .records_of("allowance_ledgers")
        .await?
        .is_empty());
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
    let (_, payer, shop) = Box::pin(wallets(
        w,
        payer_store.clone(),
        seller_store.clone(),
        AppScopes::new().allowance_execute().build()?,
        &rail,
    ))
    .await?;
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
