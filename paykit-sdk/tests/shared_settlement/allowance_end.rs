//! Enrolled payer and seller wallets follow the proposal's withdrawal and end rules.
use super::*;
use paykit_lib::allowances::AllowanceTerms;
use paykit_sdk::{AllowanceRole, AllowanceStatus, NoticeKind, Refusal};
use pubky_dispatch_client::app::{AppEvent, RecordApplication, UserConfirmed};

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
    w.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_dispatch_core::records::DAY_MS);
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
async fn test_enrolled_payer_withdraws_at_the_installed_version_after_a_size_refusal() -> TestResult
{
    Box::pin(terminal_after_loss(RecoveredProposal::UploadWithoutProof)).await
}

#[tokio::test]
async fn test_enrolled_payer_recognizes_only_its_withdrawn_allowances_return() -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::Unrelated)).await
}

#[tokio::test]
async fn test_enrolled_payer_keeps_the_matching_return_without_a_local_tombstone() -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::Collected)).await
}

#[tokio::test]
async fn test_enrolled_payer_holds_a_missing_proposal_after_loss() -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::Missing)).await
}

#[tokio::test]
async fn test_enrolled_payer_reports_loss_with_the_live_proposal_but_no_original_create(
) -> TestResult {
    Box::pin(terminal_after_loss(RecoveredProposal::MissingCreate)).await
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
    Unrelated,
    Collected,
    Original,
    UploadWithoutProof,
    Missing,
    MissingCreate,
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
    if let Some(withdrawn) = Box::pin(receive_fenced_acceptance(w, case)).await? {
        Box::pin(receive_withdrawal_return(w, withdrawn)).await?;
    }
    Ok(())
}

struct Withdrawn {
    case: RecoveredProposal,
    rail: Arc<FakeRail>,
    payer_store: Arc<MemoryStore>,
    payer: Paykit,
    shop: Paykit,
    offer: paykit_sdk::SentAllowance,
    pending_key: [u8; 32],
    mirror: [u8; 32],
    unrelated: Option<[u8; 32]>,
    end: String,
    answer: String,
}

// A second enrolled app uploads the original signed write without its service proof.
// The payer keeps that proof, with enough other records that its own upload exceeds
// the normal 16 MiB request limit (Dispatch wire 7.1, 7.4.14).
async fn retain_large_inventory(payer: &mut Paykit) -> TestResult {
    use pubky_dispatch_client::app::{Collection, RecordTarget};
    let collection = Collection::new(
        "notes",
        pubky_dispatch_core::ident::Policy::new("paykit_allowance", &["paykit_allowance"]),
    );
    let value = serde_json::to_vec(&"a".repeat(200 << 10))?;
    for number in 0u8..36 {
        let target = RecordTarget::new(collection.clone(), &[number]);
        payer.app_mut().put_record(&target, &value, vec![]).await?;
    }
    payer.app_mut().retain_loss_uploads().await?;
    Ok(())
}

async fn upload_without_proof(
    world: &World,
    phone: &mut Paykit,
    origin: &str,
    opening: pubky_dispatch_core::sig::SignedDoc,
    original: &pubky_dispatch_client::app::KeptOp,
) -> TestResult {
    use pubky_dispatch_client::client::answers::{RecoveryUpload, UploadedOp};
    use pubky_dispatch_client::client::{Credentials, SwitchClient};
    phone.app_mut().refresh_identity().await?;
    phone.app_mut().follow_provider().await?;
    let buyer = world.identity("buyer")?;
    let client = SwitchClient::new(
        origin,
        buyer.public(),
        Credentials::App {
            identity: buyer.public(),
            keys: world.app_keys(&buyer, "phone")?,
            cert: phone.app().cert().clone(),
        },
        Some(phone.app().own_identity().body().inbox.service_key),
        world.transport(),
        world.clock().clone(),
    )?;
    let answer = client
        .recovery_upload(
            &RecoveryUpload {
                lost_identity: Some(opening),
                records: vec![UploadedOp {
                    op: original.op.clone(),
                    cert: original.cert.clone(),
                    proof: None,
                }],
                ..Default::default()
            },
            true,
        )
        .await?;
    assert!(answer.proof.is_some());
    Ok(())
}

async fn check_incomplete_backup(
    w: &World,
    payer: &mut Paykit,
    target: &pubky_dispatch_client::app::RecordTarget,
    offer: &paykit_sdk::SentAllowance,
    seller: &[u8; 32],
    rail: &Arc<FakeRail>,
) -> TestResult<Vec<u8>> {
    let kept = payer.app().protocol_scan("paykit", b"ap/").await?;
    let [(key, _)] = kept.as_slice() else {
        panic!("one prepared withdrawal")
    };
    let backup = payer.backup().await?;
    let mut restored = Box::pin(Paykit::restore(
        w.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        &backup,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    Box::pin(finish_restored_withdrawal(&mut restored)).await?;
    assert!(restored
        .app_mut()
        .read_record_application(target)
        .await?
        .is_none());
    assert!(matches!(
        Box::pin(restored.end_allowance(&offer.record))
            .await
            .unwrap_err()
            .refusal(),
        Some(Refusal::HeldForUser(_))
    ));
    assert_eq!(restored.app().protocol_scan("paykit", b"ap/").await?, kept);
    assert!(restored
        .app()
        .sent_to(seller, 0, u64::MAX)
        .await?
        .iter()
        .all(|message| message.kind != "paykit.allowance_end"));
    Ok(key.clone())
}

async fn receive_fenced_acceptance(
    w: &World,
    case: RecoveredProposal,
) -> TestResult<Option<Box<Withdrawn>>> {
    use pubky_dispatch_client::app::{Collection, RecordTarget};
    use pubky_dispatch_core::ops::Condition;

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
    let unrelated = if case == RecoveredProposal::Unrelated {
        let other =
            Box::pin(payer.propose_executable_allowance(peer, terms()?, UserConfirmed::new()))
                .await?;
        Box::pin(shop.receive(None)).await?;
        let mirror = shop
            .allowances()
            .await?
            .into_iter()
            .find(|v| v.allowance_id == other.allowance_id)
            .ok_or("missing unrelated seller proposal")?
            .record;
        Box::pin(shop.accept_allowance(&mirror)).await?;
        Box::pin(payer.receive(None)).await?;
        assert_eq!(
            payer.allowance(&other.record).await?.status,
            AllowanceStatus::Accepted
        );
        Some(mirror)
    } else {
        None
    };
    let mut phone = if case == RecoveredProposal::UploadWithoutProof {
        let phone = Box::pin(join(
            w,
            &mut signer,
            "buyer",
            "phone",
            AppScopes::new().allowance().build()?,
            Arc::new(MemoryStore::new()),
            &rail,
        ))
        .await?;
        payer.app_mut().refresh_identity().await?;
        Some(phone)
    } else {
        None
    };
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
    let RecordApplication {
        record: original,
        application: created_at,
        evidence: original_op,
        ..
    } = payer
        .app_mut()
        .read_record_application(&target)
        .await?
        .ok_or("missing original pending")?;
    assert!(original.authenticated && original_op.proof.is_some());
    assert_eq!(created_at.generation, original_generation);
    assert_eq!(original.writer, payer.app_id());
    if case == RecoveredProposal::MissingCreate {
        // The same app imports its older backup while G1 can still authenticate P.
        // Its new store learns P from the service without the original create receipt.
        drop(payer);
        payer = Box::pin(Paykit::restore(
            w.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            &before_proposal,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        Box::pin(finish_restored_withdrawal(&mut payer)).await?;
        let RecordApplication {
            record: known,
            application: source,
            ..
        } = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .ok_or("older backup could not read the original live P")?;
        assert!(known.authenticated);
        assert_eq!(source, created_at);
        assert!(payer
            .app()
            .protocol_scan("paykit", b"ao/")
            .await?
            .is_empty());
    }
    let opening = payer.app().own_identity().signed().clone();
    if phone.is_some() {
        Box::pin(retain_large_inventory(&mut payer)).await?;
    }
    let next = "https://replacement.buyer.allowance.test";
    w.lose_switch("https://buyer.allowance.test");
    w.add_switch(next, &[&w.identity("buyer")?]).await?;
    Box::pin(signer.declare_provider_loss(
        next,
        pubky_dispatch_signer::UserConfirmed::shown_and_confirmed(),
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
    if let Some(phone) = phone.as_mut() {
        Box::pin(upload_without_proof(w, phone, next, opening, &original_op)).await?;
    }
    payer.app_mut().refresh_identity().await?;
    payer.app_mut().follow_provider().await?;
    if phone.is_some() {
        let uploads = payer
            .app()
            .loss_uploads(payer.app().own_identity().generation())
            .await?;
        assert!(uploads.iter().any(|upload| matches!(
            upload.outcome,
            pubky_dispatch_client::app::LossUploadOutcome::TooLarge
        )));
    }
    Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    assert_eq!(
        payer.app().own_identity().body().recovery_from,
        Some(original_generation)
    );
    payer.app_mut().invite(&peer).await?;
    shop.app_mut().identity_of(&payer.identity(), true).await?;
    let expected_version = if case == RecoveredProposal::UploadWithoutProof {
        assert!(original.version.version > 1);
        pubky_dispatch_core::ops::RecordVersion::new(original_generation, 1)
    } else {
        original.version
    };
    let selection = if case != RecoveredProposal::Missing {
        let restored = payer.app_mut().read_record_fresh(&target).await?;
        let live = match restored {
            pubky_dispatch_client::app::RecordRead::Fresh(live) => live,
            pubky_dispatch_client::app::RecordRead::Kept { reason, record } => {
                return Err(format!(
                    "replacement pending read: {reason:?}, cached {}",
                    record.is_some()
                )
                .into())
            }
            _ => return Err("unrecognized pending read".into()),
        };
        assert_eq!(live.content, original.content);
        let selection = payer.app().record_condition(&pending_key).await?;
        assert!(
            matches!(&selection, Condition::RecordCurrent { key, revision, deleted: false }
            if *key == pending_key && revision.generation == payer.app().own_identity().generation()
                && revision.item_hash == created_at.item_hash)
        );
        let RecordApplication {
            application: installed,
            evidence: member,
            installed_version,
            ..
        } = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .expect("installed proposal");
        assert_eq!(member, original_op);
        assert_eq!(installed_version, expected_version);
        assert_eq!(live.version, original.version);
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
            .delete_record(
                &target,
                vec![selection.ok_or("missing original selection")?],
            )
            .await?;
        let condition = payer.app().record_condition(&pending_key).await?;
        let later = payer
            .app_mut()
            .put_record(&target, pending, vec![condition])
            .await?;
        assert_ne!(later.version, original.version);
        Box::pin(payer.receive(None)).await?;
        Box::pin(payer.flush()).await?;
        let RecordApplication {
            record: retained, ..
        } = payer
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
        let RecordApplication {
            application: revision,
            ..
        } = payer
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
        assert!(payer
            .resume_allowance_executor(&offer.record)
            .await
            .is_err());
        assert!(rail.all_executions().is_empty());
        return Ok(None);
    }
    if case == RecoveredProposal::Interrupted {
        w.lose_answer("POST", "/ops");
    } else if case == RecoveredProposal::UploadWithoutProof {
        w.unreachable("POST", "/ops");
    }
    let read = Box::pin(payer.receive(None)).await?;
    assert!(read
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived {msg_id,..} if msg_id == &answer)));
    if case == RecoveredProposal::MissingCreate {
        let notices = payer.notices().await?;
        let notice = notices
            .iter()
            .find(|n| {
                n.kind == NoticeKind::AllowanceAcceptanceBeforeLoss
                    && n.msg_id.as_deref() == Some(answer.as_str())
            })
            .ok_or("missing permanent-refusal notice")?;
        payer.acknowledge_notice(&notice.id).await?;
        Box::pin(payer.receive(None)).await?;
        Box::pin(payer.flush()).await?;
        assert!(payer.notices().await?.iter().all(|n| n.id != notice.id));
        let RecordApplication {
            application: installed,
            evidence: member,
            ..
        } = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .ok_or("original was deleted without authority")?;
        assert_eq!(installed.item_hash, created_at.item_hash);
        assert_eq!(member, original_op);
        assert!(payer
            .app()
            .protocol_scan("paykit", b"ao/")
            .await?
            .is_empty());
        assert!(payer
            .app()
            .sent_to(&peer, 0, u64::MAX)
            .await?
            .iter()
            .all(|sent| sent.kind != "paykit.allowance_end"));
        assert!(payer.app().shared_record(&offer.record).await?.is_none());
        assert!(payer
            .app()
            .records_of("allowance_ledgers")
            .await?
            .is_empty());
        assert!(payer
            .resume_allowance_executor(&offer.record)
            .await
            .is_err());
        assert_eq!(
            shop.allowance(&mirror).await?.status,
            AllowanceStatus::Accepted
        );
        assert!(rail.all_executions().is_empty());
        return Ok(None);
    }
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
        assert!(payer
            .resume_allowance_executor(&offer.record)
            .await
            .is_err());
        assert!(rail.all_executions().is_empty());
        return Ok(None);
    }
    assert!(payer
        .notices()
        .await?
        .iter()
        .any(|n| n.kind == NoticeKind::AllowanceAcceptanceBeforeLoss
            && n.msg_id.as_deref() == Some(&answer)));
    let prepared_key = if case == RecoveredProposal::UploadWithoutProof {
        let before_delete = payer
            .app_mut()
            .read_record_application(&target)
            .await?
            .ok_or("the unanswered submission changed the live proposal")?;
        assert_eq!(before_delete.installed_version, expected_version);
        assert_eq!(before_delete.application.item_hash, created_at.item_hash);
        assert!(before_delete.record.content.value().is_some());
        Some(
            Box::pin(check_incomplete_backup(
                w, &mut payer, &target, &offer, &peer, &rail,
            ))
            .await?,
        )
    } else {
        None
    };
    if matches!(
        case,
        RecoveredProposal::Interrupted | RecoveredProposal::UploadWithoutProof
    ) {
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
        .expect("receiving the fenced original acceptance withdraws the exact live proposal");
    if let Some(key) = prepared_key {
        assert!(key.ends_with(pubky_dispatch_core::enc::b64(&end.plaintext.hash()).as_bytes()));
    }
    let end = end.msg_id.clone();
    let RecordApplication {
        record: deleted,
        application: at,
        evidence: deletion,
        ..
    } = payer
        .app_mut()
        .read_record_application(&target)
        .await?
        .ok_or("missing applied pending deletion")?;
    assert!(deleted.authenticated && deletion.proof.is_some() && deleted.content.value().is_none());
    assert_eq!(at.generation, payer.app().own_identity().generation());
    let op = pubky_dispatch_core::ops::SignedOp::verify_with_cert(
        &deletion.op,
        payer.app().cert(),
        &payer.identity(),
    )?;
    assert!(op.body().conditions.contains(&selection));
    assert!(
        matches!(&op.body().kind, pubky_dispatch_core::ops::OpKind::RecordPut(put)
        if put.key == pending_key && put.expected_version == expected_version)
    );
    Ok(Some(Box::new(Withdrawn {
        case,
        rail,
        payer_store,
        payer,
        shop,
        offer,
        pending_key,
        mirror,
        unrelated,
        end,
        answer,
    })))
}

async fn receive_withdrawal_return(w: &World, withdrawn: Box<Withdrawn>) -> TestResult {
    use pubky_dispatch_client::app::{Collection, RecordTarget};
    let Withdrawn {
        case,
        rail,
        mut payer_store,
        mut payer,
        mut shop,
        offer,
        pending_key,
        mirror,
        unrelated,
        end,
        answer,
    } = *withdrawn;
    let peer = shop.identity();
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
    assert!(payer
        .notices()
        .await?
        .iter()
        .all(|n| n.kind != NoticeKind::AllowanceWithdrawalConfirmed));
    if let Some(other) = unrelated {
        let at = format!("ao/{}", pubky_dispatch_core::enc::b64(&offer.record));
        let protected = payer
            .app()
            .protocol_get("paykit", at.as_bytes())
            .await?
            .ok_or("missing protected withdrawal")?;
        let id = Box::pin(shop.end_allowance(&other)).await?;
        let read = Box::pin(payer.receive(None)).await?;
        assert!(read
            .report
            .events
            .iter()
            .any(|e| matches!(e,AppEvent::MessageReceived{msg_id,..} if msg_id==&id)));
        assert!(payer
            .notices()
            .await?
            .iter()
            .all(|n| n.kind != NoticeKind::AllowanceWithdrawalConfirmed));
        assert_eq!(
            payer
                .app()
                .protocol_get("paykit", at.as_bytes())
                .await?
                .as_ref(),
            Some(&protected)
        );
    }
    if case == RecoveredProposal::Collected {
        // Put the applied deletion behind the backed-up cursor before the shop replies.
        Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    }
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
    let returned_id = &ended.message().msg_id;
    let protected_key = format!("ao/{}", pubky_dispatch_core::enc::b64(&offer.record));
    if case == RecoveredProposal::Collected {
        (payer, payer_store) = Box::pin(receive_return_without_tombstone(
            w,
            &mut payer,
            &rail,
            &pending_key,
            &protected_key,
            returned_id,
        ))
        .await?;
    } else {
        let read = Box::pin(payer.receive(None)).await?;
        assert!(read.report.events.iter().any(
            |event| matches!(event, AppEvent::MessageReceived {msg_id,..} if msg_id == returned_id)
        ));
    }
    let receipt = payer
        .notices()
        .await?
        .into_iter()
        .find(|n| {
            n.kind == NoticeKind::AllowanceWithdrawalConfirmed
                && n.msg_id.as_deref() == Some(ended.message().msg_id.as_str())
        })
        .ok_or("the wallet did not recognize the seller's matching signed return")?;
    payer.acknowledge_notice(&receipt.id).await?;
    if case == RecoveredProposal::Collected {
        (payer, payer_store) = Box::pin(redeliver_return(
            w,
            &mut payer,
            &mut shop,
            &rail,
            &protected_key,
            returned_id,
        ))
        .await?;
    }
    drop(payer);
    payer = Box::pin(Paykit::load(
        w.app_env(payer_store),
        Default::default(),
        id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    Box::pin(payer.receive(None)).await?;
    Box::pin(payer.flush()).await?;
    assert!(payer.notices().await?.iter().all(|n| n.id != receipt.id));
    assert_eq!(
        payer
            .app()
            .sent_to(&peer, 0, u64::MAX)
            .await?
            .iter()
            .filter(|m| m.kind == "paykit.allowance_end")
            .count(),
        usize::from(case != RecoveredProposal::Collected)
    );
    assert!(payer
        .resume_allowance_executor(&offer.record)
        .await
        .is_err());
    assert!(matches!(
        payer.allowance(&offer.record).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    assert_eq!(
        ended.message().refs,
        [stored.proposal.hash(), response.hash()]
    );
    let ledgers = paykit_lib::Collection::AllowanceLedgers;
    let ledger = RecordTarget::new(
        Collection::new(ledgers.name(), ledgers.policy(None)),
        &natural_key::allowance(&peer, &offer.allowance_id),
    );
    assert!(payer
        .app()
        .shared_record(&payer.app().record_key(&ledger)?)
        .await?
        .is_none());
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn restore_without_cached_records(
    w: &World,
    payer: &mut Paykit,
    rail: &Arc<FakeRail>,
) -> TestResult<(Paykit, Arc<MemoryStore>)> {
    let backup = payer.backup().await?;

    let payer_store = Arc::new(MemoryStore::new());
    let payer = Box::pin(Paykit::restore(
        w.app_env(payer_store.clone()),
        Default::default(),
        &backup,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    Ok((payer, payer_store))
}

async fn receive_return_without_tombstone(
    w: &World,
    payer: &mut Paykit,
    rail: &Arc<FakeRail>,
    pending_key: &[u8; 32],
    protected_key: &str,
    returned_id: &str,
) -> TestResult<(Paykit, Arc<MemoryStore>)> {
    let id = payer.app_id();
    // A fresh installation resumes at the backed-up cursor; its collection
    // listing excludes deleted records. Its Paykit layer retains the proved D.
    let (mut payer, payer_store) = Box::pin(restore_without_cached_records(w, payer, rail)).await?;
    for _ in 0..8 {
        assert!(payer.app().shared_record(pending_key).await?.is_none());
        let read = Box::pin(payer.app_mut().receive(None)).await?;
        if read
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageReceived{msg_id,..} if msg_id==returned_id))
        {
            break;
        }
        assert!(read.more, "{read:?}");
    }
    let protected = payer
        .app()
        .protocol_get("paykit", protected_key.as_bytes())
        .await?
        .ok_or("missing original")?;
    let held: serde_json::Value = serde_json::from_slice(&protected)?;
    assert!(
        held["d"]["settlement"]["returned"]["Received"].is_object(),
        "the exact seller return should be protected before the fresh lifetime reads"
    );
    assert!(payer
        .app()
        .protocol_scan("paykit", b"ae/")
        .await?
        .is_empty());
    assert!(payer
        .notices()
        .await?
        .iter()
        .all(|n| n.kind != NoticeKind::AllowanceWithdrawalConfirmed));
    drop(payer);
    payer = Box::pin(Paykit::load(
        w.app_env(payer_store.clone()),
        Default::default(),
        id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    assert_eq!(
        payer
            .app()
            .protocol_get("paykit", protected_key.as_bytes())
            .await?
            .as_ref(),
        Some(&protected)
    );
    Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    Ok((payer, payer_store))
}

async fn redeliver_return(
    w: &World,
    payer: &mut Paykit,
    shop: &mut Paykit,
    rail: &Arc<FakeRail>,
    protected_key: &str,
    returned_id: &str,
) -> TestResult<(Paykit, Arc<MemoryStore>)> {
    let terminal = payer
        .app()
        .protocol_get("paykit", protected_key.as_bytes())
        .await?
        .ok_or("missing retired original")?;
    let (mut payer, payer_store) = Box::pin(restore_without_cached_records(w, payer, rail)).await?;
    Box::pin(finish_restored_withdrawal(&mut payer)).await?;
    assert!(matches!(
        shop.app_mut().resend(returned_id).await?,
        pubky_dispatch_client::app::Delivery::Accepted { .. }
    ));
    let repeat = Box::pin(payer.receive(None)).await?;
    assert!(
        matches!(
            repeat.report.events.as_slice(),
            [AppEvent::Duplicate {
                conflicting: false,
                ..
            }]
        ),
        "{repeat:?}"
    );
    assert_eq!(
        payer
            .app()
            .protocol_get("paykit", protected_key.as_bytes())
            .await?
            .as_ref(),
        Some(&terminal)
    );
    Ok((payer, payer_store))
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
