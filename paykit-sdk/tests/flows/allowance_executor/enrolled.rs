//! The approving wallet's first grant after ordinary enrollment and acceptance.

use std::error::Error;
use std::sync::Arc;

use paykit_lib::{records::AllowanceRecord, Collection};
use paykit_sdk::{AppScopes, Paykit, Rail, RequestedScopes};
use pubky_switch_client::app::{AppEnv, UserConfirmed};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::transport::{BoxFuture, Request, Response, Transport, TransportError};
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{Protocols, Signer};

use super::{applied_put, terms, AllowanceRole, AllowanceStatus, Condition, Ledger};
use crate::common::{finish_listing, TestTimer};

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

fn app_env(world: &World, storage: Arc<MemoryStore>) -> AppEnv {
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
    storage: Arc<MemoryStore>,
    rail: Arc<dyn Rail>,
) -> TestResult<Paykit> {
    let identity = world.identity(name)?;
    let origin = format!("https://{name}.allowance.test");
    Box::pin(world.add_switch(&origin, &[&identity])).await?;
    let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    if matches!(world.backend(), Backend::Service) {
        env.transport = Arc::new(ServiceTask(env.transport));
    }
    let keys = world.keyring(&identity)?;
    let origin_for_signer = origin.clone();
    let mut signer = tokio::task::spawn_local(async move {
        let mut signer = Box::pin(Signer::create(env, keys, &origin_for_signer)).await?;
        Box::pin(signer.resume()).await?;
        Ok::<_, Box<dyn Error>>(signer)
    })
    .await??;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&identity, name)?, &names)?;
    let own = identity.public();
    let request = tokio::task::spawn_local(async move {
        let reviewed = signer.review(asking.request.signed(), &asking.auth)?;
        let approved = Box::pin(signer.approve(reviewed)).await?;
        signer.claim_delivered(&approved.request).await?;
        Ok::<_, Box<dyn Error>>(asking.join_request(own, &approved)?)
    })
    .await??;
    world.advance_ms(1_000);
    let env = app_env(world, storage);
    let timer = Arc::new(TestTimer {
        clock: world.clock().clone(),
    });
    tokio::task::spawn_local(async move {
        let mut wallet =
            Box::pin(Paykit::join(env, Default::default(), request, rail, timer)).await?;
        Box::pin(finish_listing(&mut wallet)).await;
        Ok::<_, Box<dyn Error>>(wallet)
    })
    .await?
}

async fn grant(world: &World, proposer: AllowanceRole) -> TestResult {
    let storage = Arc::new(MemoryStore::new());
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
        AppScopes::new().allowance().request().build()?,
        Arc::new(MemoryStore::new()),
        rail.clone(),
    ))
    .await?;
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
        AllowanceRole::Allower => (&mut seller, payer.identity()),
        AllowanceRole::Allowee => (&mut payer, seller.identity()),
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
            Box::pin(finish_listing(&mut payer)).await;
        }
        AllowanceRole::Allowee => {
            Box::pin(payer.accept_executable_allowance(&key, UserConfirmed::new())).await?;
            Box::pin(finish_listing(&mut seller)).await;
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
    let natural =
        paykit_lib::derive::natural_key::allowance(&seller.identity(), &proposal.allowance_id);
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
                &proposal.allowance_id,
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
        app_env(world, storage),
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
    Ok(())
}

#[tokio::test]
async fn test_enrolled_wallet_first_grant_on_both_services() -> TestResult {
    tokio::task::LocalSet::new()
        .run_until(async {
            for backend in [Backend::Scripted, Backend::Service] {
                for proposer in [AllowanceRole::Allower, AllowanceRole::Allowee] {
                    let Some(world) = World::start(backend)? else {
                        continue;
                    };
                    let result = Box::pin(grant(&world, proposer)).await;
                    world.close().await?;
                    result?;
                }
            }
            Ok(())
        })
        .await
}
