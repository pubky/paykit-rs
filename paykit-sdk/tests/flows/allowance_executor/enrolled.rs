//! The approving wallet's first grant after ordinary enrollment and acceptance.

use std::error::Error;
use std::sync::Arc;

use paykit_lib::{records::AllowanceRecord, Collection};
use paykit_sdk::{AppScopes, Paykit, PaykitBackup, Rail, RequestedScopes};
use pubky_switch_client::app::{AppEnv, KeptOp, UserConfirmed};
use pubky_switch_client::store::{Fault, FaultStore, MemoryStore, StateStore};
use pubky_switch_client::transport::{BoxFuture, Request, Response, Transport, TransportError};
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{Protocols, Signer};

mod payments;
mod sibling;

use super::{applied_put, terms, AllowanceRole, AllowanceStatus, Condition, Ledger};
use crate::common::{finish_listing, ListsStore, TestTimer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

// The real service handles each request on a separate task, as at its HTTP boundary.
struct ServiceTask(Arc<dyn Transport>);

impl Transport for ServiceTask {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        let service = self.0.clone();
        Box::pin(async move {
            tokio::spawn(async move { service.send(request).await })
                .await
                .expect("service task completed")
        })
    }
}

fn app_env(world: &World, storage: Arc<dyn StateStore>) -> AppEnv {
    let mut env = world.app_env(storage);
    if matches!(world.backend(), Backend::Service) {
        env.transport = Arc::new(ServiceTask(env.transport));
    }
    env
}

async fn join(
    world: &World,
    name: &str,
    scopes: RequestedScopes,
    storage: Arc<dyn StateStore>,
    rail: Arc<dyn Rail>,
) -> TestResult<Paykit> {
    let signer = Box::pin(start_signer(world, name)).await?;
    let (_, wallet) = Box::pin(join_app(world, name, name, signer, scopes, storage, rail)).await?;
    Ok(wallet)
}

async fn start_signer(world: &World, name: &str) -> TestResult<Signer> {
    let identity = world.identity(name)?;
    let origin = format!("https://{name}.allowance.test");
    Box::pin(world.add_switch(&origin, &[&identity])).await?;
    let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    if matches!(world.backend(), Backend::Service) {
        env.transport = Arc::new(ServiceTask(env.transport));
    }
    let keys = world.keyring(&identity)?;
    tokio::task::spawn_local(async move {
        let mut signer = Box::pin(Signer::create(env, keys, &origin)).await?;
        Box::pin(signer.resume()).await?;
        Ok::<_, Box<dyn Error>>(signer)
    })
    .await?
}

async fn join_app(
    world: &World,
    name: &str,
    app_name: &str,
    mut signer: Signer,
    scopes: RequestedScopes,
    storage: Arc<dyn StateStore>,
    rail: Arc<dyn Rail>,
) -> TestResult<(Signer, Paykit)> {
    let identity = world.identity(name)?;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&identity, app_name)?, &names)?;
    let own = identity.public();
    let (signer, request) = tokio::task::spawn_local(async move {
        let reviewed = signer.review(asking.request.signed(), &asking.auth)?;
        let approved = Box::pin(signer.approve(reviewed)).await?;
        signer.claim_delivered(&approved.request).await?;
        Ok::<_, Box<dyn Error>>((signer, asking.join_request(own, &approved)?))
    })
    .await??;
    world.advance_ms(1_000);
    let env = app_env(world, storage);
    let timer = Arc::new(TestTimer {
        clock: world.clock().clone(),
    });
    let wallet = tokio::task::spawn_local(async move {
        let mut wallet =
            Box::pin(Paykit::join(env, Default::default(), request, rail, timer)).await?;
        Box::pin(finish_listing(&mut wallet)).await;
        Ok::<_, Box<dyn Error>>(wallet)
    })
    .await??;
    Ok((signer, wallet))
}

#[derive(Clone, Copy)]
enum GrantFlow {
    FirstGrant,
    OlderBackup([&'static str; 3]),
    LatePeriodProof,
    UnansweredReservation,
}

struct GrantedWallets {
    payer: Paykit,
    seller: Paykit,
    storage: Arc<ListsStore>,
    rail: Arc<paykit_sdk::rail::FakeRail>,
    key: [u8; 32],
    pending: Option<PaykitBackup>,
}

async fn grant(
    world: &World,
    proposer: AllowanceRole,
    flow: GrantFlow,
) -> TestResult<GrantedWallets> {
    let storage = Arc::new(ListsStore::default());
    let rail = Arc::new(paykit_sdk::rail::FakeRail::with_clock(
        world.clock().clone(),
    ));
    let mut payer = Box::pin(join(
        world,
        "payer",
        AppScopes::new().allowance_execute().build()?,
        storage.clone(),
        rail.clone(),
    ))
    .await?;
    let mut seller = Box::pin(join(
        world,
        "seller",
        AppScopes::new().allowance().request().build()?,
        Arc::new(MemoryStore::new()),
        rail.clone(),
    ))
    .await?;
    let (key, allowance_id) = Box::pin(accept(&mut payer, &mut seller, proposer)).await?;
    let permanent = payer
        .app()
        .shared_record(&key)
        .await?
        .expect("original accepted record");
    let bound = AllowanceRecord::bind(
        permanent.content.value().unwrap(),
        &payer.identity(),
        payer.app().naming_key(),
        &key,
    )?;
    let natural = paykit_lib::derive::natural_key::allowance(&seller.identity(), &allowance_id);
    let ledger_key =
        Collection::AllowanceLedgers.record_key(payer.app().naming_key(), &natural, None)?;
    let ledger = payer
        .app()
        .shared_record(&ledger_key)
        .await?
        .expect("original zero ledger");
    assert_eq!(
        ledger.content.value(),
        Some(
            Ledger::initial_value(
                &seller.identity(),
                &allowance_id,
                &bound.response.as_ref().unwrap().hash()
            )
            .as_slice()
        )
    );
    let (original, permanent_revision) = applied_put(payer.app(), key, permanent.version).await;
    let (zero, ledger_revision) = applied_put(payer.app(), ledger_key, ledger.version).await;
    assert_eq!(original.body().batch, zero.body().batch);
    assert_eq!(original.body().batch.count, 3);
    assert_eq!(permanent_revision.seq + 1, ledger_revision.seq);
    let pending = if matches!(flow, GrantFlow::OlderBackup(_)) {
        world.lose_answer("POST", "/ops");
        assert!(Box::pin(payer.resume_allowance_executor(&key))
            .await
            .is_err());
        Box::pin(payer.receive(None)).await?;
        let at = format!("lv/{}", pubky_switch_core::enc::b64(&key));
        let saved = payer
            .app()
            .protocol_get("paykit", at.as_bytes())
            .await?
            .unwrap();
        let history: serde_json::Value = serde_json::from_slice(&saved)?;
        assert_eq!(history["d"][0]["grant"]["kind"], "awaiting_proof");
        Some(Box::pin(payer.backup()).await?)
    } else {
        None
    };
    Box::pin(payer.resume_allowance_executor(&key)).await?;
    let granted = payer.app().shared_record(&key).await?.expect("first grant");
    let (write, _) = applied_put(payer.app(), key, granted.version).await;
    for (key, revision) in [(key, permanent_revision), (ledger_key, ledger_revision)] {
        assert!(write.body().conditions.contains(&Condition::RecordCurrent {
            key,
            revision,
            deleted: false
        }));
    }
    let app_id = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(Paykit::load(
        app_env(world, storage.clone()),
        Default::default(),
        app_id,
        rail.clone(),
        Arc::new(TestTimer {
            clock: world.clock().clone(),
        }),
    ))
    .await?;
    Box::pin(payer.resume_allowance_executor(&key)).await?;
    let view = payer.allowance(&key).await?;
    assert_eq!(view.executor_app_id, Some(app_id));
    assert!(view.consented);
    assert_eq!(
        payer.app().shared_record(&key).await?.unwrap().version,
        granted.version
    );
    assert!(rail.all_executions().is_empty());
    Ok(GrantedWallets {
        payer,
        seller,
        storage,
        rail,
        key,
        pending,
    })
}

// Finish enrollment before polling a payment so its Switch reads have the wallet stack.
async fn run_grant_flow(world: &World, proposer: AllowanceRole, flow: GrantFlow) -> TestResult {
    let GrantedWallets {
        mut payer,
        mut seller,
        storage,
        rail,
        key,
        pending,
    } = Box::pin(grant(world, proposer, flow)).await?;
    if matches!(flow, GrantFlow::UnansweredReservation) {
        Box::pin(payments::unanswered_reservation(
            world, payer, seller, storage, rail, key,
        ))
        .await?;
    } else if let GrantFlow::OlderBackup(amounts) = flow {
        Box::pin(payments::payment_after_restore(
            world,
            (payer, seller),
            storage,
            rail,
            key,
            pending.expect("original grant backup"),
            amounts,
        ))
        .await?;
    } else if matches!(flow, GrantFlow::LatePeriodProof) {
        Box::pin(payments::late_period_proof(
            world,
            &mut payer,
            &mut seller,
            &rail,
            &key,
        ))
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_enrolled_wallet_first_grant_on_both_services() -> TestResult {
    enrolled_wallets(GrantFlow::FirstGrant).await
}

#[tokio::test]
async fn test_enrolled_allowance_payments_after_an_older_grant_backup() -> TestResult {
    enrolled_wallets(GrantFlow::OlderBackup(["70", "31", "30"])).await
}

#[tokio::test]
async fn test_enrolled_rounded_allowance_payments_after_an_older_grant_backup() -> TestResult {
    enrolled_wallets(GrantFlow::OlderBackup([
        "69.9999999991",
        "30.000000001",
        "29.9999999991",
    ]))
    .await
}

#[tokio::test]
async fn test_enrolled_unanswered_reservation_on_scripted_switch() -> TestResult {
    enrolled_on(Backend::Scripted, GrantFlow::UnansweredReservation).await
}

#[tokio::test]
async fn test_enrolled_unanswered_reservation_on_switch_service() -> TestResult {
    enrolled_on(Backend::Service, GrantFlow::UnansweredReservation).await
}

#[tokio::test]
async fn test_enrolled_late_period_proof_on_scripted_switch() -> TestResult {
    enrolled_on(Backend::Scripted, GrantFlow::LatePeriodProof).await
}

#[tokio::test]
async fn test_enrolled_late_period_proof_on_switch_service() -> TestResult {
    enrolled_on(Backend::Service, GrantFlow::LatePeriodProof).await
}

async fn enrolled_wallets(flow: GrantFlow) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        enrolled_on(backend, flow).await?;
    }
    Ok(())
}

async fn enrolled_on(backend: Backend, flow: GrantFlow) -> TestResult {
    tokio::task::LocalSet::new()
        .run_until(async {
            for proposer in [AllowanceRole::Allower, AllowanceRole::Allowee] {
                let Some(world) = World::start(backend)? else {
                    continue;
                };
                let result = Box::pin(run_grant_flow(&world, proposer, flow)).await;
                world.close().await?;
                result?;
            }
            Ok(())
        })
        .await
}

async fn accept(
    payer: &mut Paykit,
    seller: &mut Paykit,
    proposer: AllowanceRole,
) -> TestResult<([u8; 32], paykit_lib::identifiers::AllowanceId)> {
    payer.app_mut().allow_connect(&seller.identity()).await?;
    seller.app_mut().allow_connect(&payer.identity()).await?;
    let proposal = match proposer {
        AllowanceRole::Allower => {
            Box::pin(payer.propose_executable_allowance(
                seller.identity(),
                terms(),
                UserConfirmed::new(),
            ))
            .await?
        }
        AllowanceRole::Allowee => {
            Box::pin(seller.propose_allowance(payer.identity(), proposer, terms())).await?
        }
        _ => unreachable!("the fixture selects Allower or Allowee"),
    };
    let (receiver, peer) = match proposer {
        AllowanceRole::Allower => (&mut *seller, payer.identity()),
        AllowanceRole::Allowee => (&mut *payer, seller.identity()),
        _ => unreachable!("the fixture selects Allower or Allowee"),
    };
    Box::pin(finish_listing(receiver)).await;
    let offer = receiver
        .allowances()
        .await?
        .into_iter()
        .find(|view| view.allowance_id == proposal.allowance_id && view.counterparty == peer)
        .expect("received proposal");
    let (key, seller_key) = match proposer {
        AllowanceRole::Allower => (proposal.record, offer.record),
        AllowanceRole::Allowee => (offer.record, proposal.record),
        _ => unreachable!("the fixture selects Allower or Allowee"),
    };
    assert!(Box::pin(payer.resume_allowance_executor(&key))
        .await
        .is_err());
    match proposer {
        AllowanceRole::Allower => {
            Box::pin(seller.accept_allowance(&seller_key)).await?;
            Box::pin(finish_listing(payer)).await;
        }
        AllowanceRole::Allowee => {
            Box::pin(payer.accept_executable_allowance(&key, UserConfirmed::new())).await?;
            Box::pin(finish_listing(seller)).await;
        }
        _ => unreachable!("the fixture selects Allower or Allowee"),
    }
    assert_eq!(
        payer.allowance(&key).await?.status,
        AllowanceStatus::Accepted
    );
    assert_eq!(
        seller.allowance(&seller_key).await?.status,
        AllowanceStatus::Accepted
    );
    Ok((key, proposal.allowance_id))
}

struct IncomingAcceptance {
    payer: Paykit,
    storage: Arc<FaultStore>,
    rail: Arc<paykit_sdk::rail::FakeRail>,
    keys: [[u8; 32]; 3],
    prepared: Vec<u8>,
}

async fn incoming_acceptance(world: &World) -> TestResult<IncomingAcceptance> {
    let storage = Arc::new(FaultStore::new(Arc::new(MemoryStore::new()), Fault::None));
    let rail = Arc::new(paykit_sdk::rail::FakeRail::new());
    let mut payer = Box::pin(join(
        world,
        "payer",
        AppScopes::new().allowance_execute().build()?,
        storage.clone(),
        rail.clone(),
    ))
    .await?;
    let mut seller = Box::pin(join(
        world,
        "seller",
        AppScopes::new().allowance().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        rail.clone(),
    ))
    .await?;
    payer.app_mut().allow_connect(&seller.identity()).await?;
    seller.app_mut().allow_connect(&payer.identity()).await?;
    let offer = Box::pin(payer.propose_executable_allowance(
        seller.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await?;
    Box::pin(finish_listing(&mut seller)).await;
    let peer = seller.allowances().await?.pop().expect("incoming proposal");
    Box::pin(seller.accept_allowance(&peer.record)).await?;
    storage.refuse_puts_as_too_large(Some(b"opq/"));
    assert!(Box::pin(payer.receive(None)).await.is_err());
    let at = format!("ao/{}", pubky_switch_core::enc::b64(&offer.record));
    let prepared = payer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await?
        .unwrap();
    let original: serde_json::Value = serde_json::from_slice(&prepared)?;
    assert!(original["d"]["opening"].is_object());
    assert!(original["d"]["opening"]["applied"].is_null());
    assert!(payer
        .app()
        .protocol_scan("paykit", b"ap/")
        .await?
        .is_empty());
    let natural =
        paykit_lib::derive::natural_key::allowance(&seller.identity(), &offer.allowance_id);
    let keys = [
        Collection::Allowances,
        Collection::AllowanceLedgers,
        Collection::AllowanceProposals,
    ]
    .map(|collection| {
        collection
            .record_key(payer.app().naming_key(), &natural, None)
            .unwrap()
    });
    assert_eq!(keys[0], offer.record);
    Ok(IncomingAcceptance {
        payer,
        storage,
        rail,
        keys,
        prepared,
    })
}

async fn finish_restore(payer: &mut Paykit) -> TestResult {
    Box::pin(finish_listing(payer)).await;
    let mut finished = false;
    for _ in 0..8 {
        match Box::pin(payer.reconcile()).await {
            Ok(report) => {
                assert!(!report.payer_skipped);
                finished = true;
                break;
            }
            Err(paykit_sdk::PaykitError::Refused(paykit_sdk::Refusal::NotNow(reason)))
                if reason.contains("Call reconcile again") => {}
            Err(error) => return Err(error.into()),
        }
    }
    assert!(finished);
    Box::pin(payer.end_reconcile(UserConfirmed::new())).await?;
    Ok(())
}

async fn original_bytes(payer: &Paykit, key: &[u8; 32]) -> TestResult<Vec<u8>> {
    let at = format!("ao/{}", pubky_switch_core::enc::b64(key));
    Ok(payer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await?
        .unwrap())
}

async fn grant_without_original_application(world: &World) -> TestResult {
    let IncomingAcceptance {
        mut payer,
        storage: _,
        rail,
        keys,
        prepared,
    } = Box::pin(incoming_acceptance(world)).await?;
    assert!(Box::pin(payer.resume_allowance_executor(&keys[0]))
        .await
        .is_err());
    assert!(
        Box::pin(payer.confirm_allowance_executor(&keys[0], UserConfirmed::new()))
            .await
            .is_err()
    );
    assert_eq!(Box::pin(original_bytes(&payer, &keys[0])).await?, prepared);
    assert!(payer
        .app()
        .protocol_scan("paykit", b"lv/")
        .await?
        .is_empty());
    for key in &keys[..2] {
        assert!(payer.app().shared_record(key).await?.is_none());
    }
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn grant_after_original_application(world: &World) -> TestResult {
    let IncomingAcceptance {
        mut payer,
        storage,
        rail,
        keys,
        prepared,
    } = Box::pin(incoming_acceptance(world)).await?;
    storage.refuse_puts_as_too_large_over(Some(b"p/paykit/ao/"), prepared.len());
    let failure = Box::pin(payer.receive(None)).await.unwrap_err();
    assert!(matches!(
        failure,
        paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Store(
            pubky_switch_client::error::StoreError::TooLarge { .. }
        ))
    ));
    assert_eq!(Box::pin(original_bytes(&payer, &keys[0])).await?, prepared);
    assert!(payer
        .app()
        .protocol_scan("paykit", b"ap/")
        .await?
        .is_empty());
    let evidence = payer.app().shared_evidence().await?;
    let originals: Vec<KeptOp> = keys
        .iter()
        .map(|key| {
            evidence
                .ops
                .iter()
                .find(|kept| {
                    let cert =
                        paykit_lib::scopes::verify_certificate(&kept.cert, &payer.identity())
                            .unwrap();
                    let op = pubky_switch_core::ops::SignedOp::verify_with_cert(
                        &kept.op,
                        &cert,
                        &payer.identity(),
                    )
                    .unwrap();
                    matches!(&op.body().kind, pubky_switch_core::ops::OpKind::RecordPut(put)
                if &put.key == key && op.body().batch.count == 3 && cert.app_id() == payer.app_id())
                })
                .expect("applied original member")
                .clone()
        })
        .collect();
    assert!(originals.iter().all(|kept| kept.proof.is_some()));
    let before = [
        payer.app().shared_record(&keys[0]).await?.unwrap().version,
        payer.app().shared_record(&keys[1]).await?.unwrap().version,
    ];
    // The service applied the original before this backup. The wallet still has the
    // signed decision and prepared opening, but could not save the three results.
    let before_capture = Box::pin(payer.backup()).await?.to_bytes()?;
    let id = payer.app_id();
    drop(payer);
    storage.refuse_puts_as_too_large(None);
    let mut payer = Box::pin(Paykit::load(
        app_env(world, storage),
        Default::default(),
        id,
        rail.clone(),
        Arc::new(TestTimer {
            clock: world.clock().clone(),
        }),
    ))
    .await?;
    let captured: serde_json::Value =
        serde_json::from_slice(&Box::pin(original_bytes(&payer, &keys[0])).await?)?;
    assert_eq!(
        serde_json::from_value::<Vec<KeptOp>>(captured["d"]["opening"]["applied"].clone(),)?,
        originals
    );
    let (_, a) = applied_put(payer.app(), keys[0], before[0]).await;
    let (_, l) = applied_put(payer.app(), keys[1], before[1]).await;
    Box::pin(payer.resume_allowance_executor(&keys[0])).await?;
    let granted = payer.app().shared_record(&keys[0]).await?.unwrap();
    let (grant, _) = applied_put(payer.app(), keys[0], granted.version).await;
    for (key, revision) in [(keys[0], a), (keys[1], l)] {
        assert!(grant.body().conditions.contains(&Condition::RecordCurrent {
            key,
            revision,
            deleted: false,
        }));
    }
    Box::pin(payer.resume_allowance_executor(&keys[0])).await?;
    assert_eq!(
        payer.app().shared_record(&keys[0]).await?.unwrap().version,
        granted.version
    );
    assert_eq!(
        payer.app().shared_record(&keys[1]).await?.unwrap().version,
        before[1]
    );
    assert!(payer.allowance(&keys[0]).await?.consented);

    // The legitimate first grant replaced the original permanent member. Restore both
    // backups into empty stores against that same current grant and unchanged zero.
    let after_capture = Box::pin(payer.backup()).await?.to_bytes()?;
    rail.set_history_payer(payer.identity());
    drop(payer);
    for (backup, complete) in [(&before_capture, false), (&after_capture, true)] {
        let mut restored = Box::pin(Paykit::restore(
            app_env(world, Arc::new(MemoryStore::new())),
            Default::default(),
            &PaykitBackup::from_bytes(backup)?,
            rail.clone(),
            Arc::new(TestTimer {
                clock: world.clock().clone(),
            }),
        ))
        .await?;
        assert_eq!(restored.app_id(), id);
        Box::pin(finish_restore(&mut restored)).await?;
        let local = Box::pin(original_bytes(&restored, &keys[0])).await?;
        let view = restored.allowance(&keys[0]).await?;
        assert_eq!(view.status, AllowanceStatus::Accepted);
        assert_eq!(view.executor_app_id, Some(id));
        let resumed = Box::pin(restored.resume_allowance_executor(&keys[0])).await;
        if complete {
            assert!(
                view.consented,
                "clean backup retains the completed grant's consent"
            );
            let approval: serde_json::Value = serde_json::from_slice(&local)?;
            assert_eq!(
                serde_json::from_value::<Vec<KeptOp>>(approval["d"]["opening"]["applied"].clone())?,
                originals
            );
            resumed?;
            assert!(restored.allowance(&keys[0]).await?.consented);
        } else {
            assert!(!view.consented, "incomplete backup has no grant consent");
            assert!(resumed.is_err());
            assert_eq!(local, prepared);
            assert!(!restored
                .app()
                .shared_evidence()
                .await?
                .ops
                .contains(&originals[0]));
            let refusal =
                Box::pin(restored.confirm_allowance_executor(&keys[0], UserConfirmed::new()))
                    .await
                    .unwrap_err();
            assert!(
                matches!(refusal.refusal(), Some(paykit_sdk::Refusal::NotNow(reason))
                if reason.contains("original Allowance grant and ledger")),
                "{refusal}"
            );
            assert!(Box::pin(restored.reconcile_allowance(&keys[0]))
                .await?
                .is_none());
            assert!(!restored.allowance(&keys[0]).await?.consented);
        }
        assert_eq!(
            restored
                .app()
                .shared_record(&keys[0])
                .await?
                .unwrap()
                .version,
            granted.version
        );
        assert_eq!(
            restored
                .app()
                .shared_record(&keys[1])
                .await?
                .unwrap()
                .version,
            before[1]
        );
        assert!(rail.all_executions().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn test_enrolled_first_grant_holds_without_the_original_application() -> TestResult {
    tokio::task::LocalSet::new()
        .run_until(async {
            for backend in [Backend::Scripted, Backend::Service] {
                let Some(world) = World::start(backend)? else {
                    continue;
                };
                let result = Box::pin(grant_without_original_application(&world)).await;
                world.close().await?;
                result?;
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn test_enrolled_first_grant_uses_the_recovered_incoming_acceptance() -> TestResult {
    tokio::task::LocalSet::new()
        .run_until(async {
            for backend in [Backend::Scripted, Backend::Service] {
                let Some(world) = World::start(backend)? else {
                    continue;
                };
                let result = Box::pin(grant_after_original_application(&world)).await;
                world.close().await?;
                result?;
            }
            Ok(())
        })
        .await
}
