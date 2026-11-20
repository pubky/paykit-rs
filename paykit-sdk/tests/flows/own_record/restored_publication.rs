//! What a fresh payer sees at the first public write by a restored shop (Paykit spec 4).

use std::error::Error;
use std::sync::{Arc, Mutex};

use paykit_lib::app_record::PaykitMember;
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, PayOptions, Paykit, PaykitBackup, PaykitConfig, PaykitError, PaymentMode, Refusal,
    UnrequestedPayment,
};
use pubky_switch_client::app::{AppError, HomeserverWriter, Publication, UserConfirmed};
use pubky_switch_client::error::TransportError;
use pubky_switch_client::testing::TestHomeserver;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_core::records::{app_record_path, AppRecord};
use pubky_switch_core::sig::SignedDoc;
use pubky_switch_harness::{Backend, World};
use serde::Deserialize;

use super::{payload, publishing};
use crate::common::{btc, endpoint, enrolled_wallet, finish_listing, store, TestTimer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, PartialEq)]
enum RestoredCopy {
    Missing,
    BeforeOffer,
    AcceptedOnly,
}

/// The shop waits for this writer to return. The payer therefore reads the first durable
/// copy before the shop can publish anything else, using its own normal Homeserver reader.
struct PayerAtFirstWrite {
    files: Arc<TestHomeserver>,
    owner: [u8; 32],
    path: String,
    payer: Mutex<Option<Paykit>>,
    writes: Mutex<Vec<SignedDoc>>,
    offer_expected: bool,
}

impl HomeserverWriter for PayerAtFirstWrite {
    fn put(
        &self,
        identity: [u8; 32],
        path: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            HomeserverWriter::put(self.files.as_ref(), identity, path.clone(), bytes).await?;
            if identity == self.owner && path == self.path {
                let served = self
                    .files
                    .file(identity, &path)
                    .expect("durable app record");
                let signed = serde_json::from_slice(&served).unwrap();
                self.writes.lock().unwrap().push(signed);
                let payer = self.payer.lock().unwrap().take();
                if let Some(mut payer) = payer {
                    let expected = self.offer_expected;
                    tokio::spawn(Box::pin(async move {
                        let list = payer
                            .public_endpoints(&identity, Some(UserConfirmed::new()))
                            .await
                            .unwrap();
                        assert!(list.failures.is_empty(), "{list:?}");
                        if expected {
                            assert_eq!(list.endpoints.len(), 1, "{list:?}");
                            assert_eq!(
                                list.endpoints[0].endpoint_id,
                                endpoint("btc-lightning-bolt12")
                            );
                            assert_eq!(list.endpoints[0].payload, payload("lno1tips"));
                        } else {
                            assert!(
                                list.endpoints.is_empty(),
                                "first restored publication: {list:?}"
                            );
                            let options = PayOptions::default()
                                .with_mode(PaymentMode::Public(UserConfirmed::new()))
                                .with_unconfirmed_identity_confirmed(UserConfirmed::new());
                            let payment = UnrequestedPayment::new(identity, btc("0.001"))
                                .with_options(options);
                            let error = payer
                                .pay_without_request(payment)
                                .await
                                .outcome
                                .unwrap_err();
                            assert!(
                                matches!(error.refusal(), Some(Refusal::NoDestination(_))),
                                "{error:?}"
                            );
                        }
                    }))
                    .await
                    .unwrap();
                }
            }
            Ok(())
        })
    }

    fn delete(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        HomeserverWriter::delete(self.files.as_ref(), identity, path)
    }
}

#[derive(Deserialize)]
struct BackedUpRecords {
    record: SignedDoc,
    accepted: SignedDoc,
}

async fn restore_shop(world: &World, copy: RestoredCopy) -> TestResult {
    let id = endpoint("btc-lightning-bolt12");
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut shop = Box::pin(enrolled_wallet(
        world,
        "seller",
        AppScopes::new().request().pay().build()?,
        publishing(),
        &rail,
        store(),
    ))
    .await?;
    let identity = shop.identity();
    let path = app_record_path(&shop.app_id());
    let before_offer = world
        .files()
        .file(identity, &path)
        .ok_or("missing initial record")?;
    assert_eq!(
        shop.set_public_endpoint(id.clone(), payload("lno1tips"))
            .await?,
        Publication::Published
    );
    let offered = shop.app().record().clone();
    let served = world.files().file(identity, &path).unwrap();
    assert_eq!(serde_json::from_slice::<SignedDoc>(&served)?, offered);
    let withdraw = copy != RestoredCopy::AcceptedOnly;
    if withdraw {
        world.unreachable("POST", "/ops");
        let error = shop.remove_public_endpoint(&id).await.unwrap_err();
        assert!(
            matches!(error, PaykitError::Client(AppError::Transport(_))),
            "{error:?}"
        );
        assert!(shop.own_public_endpoints()?.is_empty());
        assert_ne!(shop.app().record(), &offered);
    }
    let bytes = shop.backup().await?.to_bytes()?;
    let backed: BackedUpRecords = serde_json::from_slice(&bytes)?;
    let backup = PaykitBackup::from_bytes(&bytes)?;
    let original = shop.app().record().clone();
    assert_eq!(backed.record, original);
    assert_eq!(backed.accepted, offered);
    drop(shop);
    if copy == RestoredCopy::BeforeOffer {
        world.files().put(identity, &path, before_offer);
    } else {
        world.files().remove(identity, &path);
    }
    let payer = Box::pin(enrolled_wallet(
        world,
        "buyer",
        AppScopes::new().pay().build()?,
        PaykitConfig::default(),
        &rail,
        store(),
    ))
    .await?;
    let observer = Arc::new(PayerAtFirstWrite {
        files: world.files().clone(),
        owner: identity,
        path: path.clone(),
        payer: Mutex::new(Some(payer)),
        writes: Mutex::new(Vec::new()),
        offer_expected: !withdraw,
    });
    let mut env = world.app_env(store());
    env.homeserver_writer = Some(observer.clone());
    let timer = Arc::new(TestTimer {
        clock: world.clock().clone(),
    });
    let mut restored = Box::pin(Paykit::restore(
        env,
        publishing(),
        &backup,
        rail.clone(),
        timer,
    ))
    .await?;
    assert_eq!(restored.app().record(), &original);
    if copy == RestoredCopy::Missing {
        world.unreachable("GET", "/feed");
        // A fresh store may need several listing pages before it can read the feed.
        for _ in 0..16 {
            let read = restored.receive(None).await;
            assert!(observer.writes.lock().unwrap().is_empty());
            if world.faults_left() == 0 {
                assert!(read.is_err(), "the unavailable feed was reported as read");
                break;
            }
            let summary = read?;
            assert!(summary.records_unlisted.is_some() && summary.report.more);
        }
        assert_eq!(
            world.faults_left(),
            0,
            "the restored shop never read the feed"
        );
    }
    if copy == RestoredCopy::BeforeOffer {
        let publication = match restored.update_own_record().await {
            Err(PaykitError::Client(AppError::Refused(
                pubky_switch_client::app::Refusal::NotNow(_),
            ))) if restored.app().restore_held_cursor().is_none() => {
                assert!(observer.writes.lock().unwrap().is_empty());
                finish_listing(&mut restored).await;
                restored.update_own_record().await?
            }
            result => result?,
        };
        assert_eq!(publication, Publication::Published);
    } else {
        restored.receive(None).await?;
    }
    let settled: BackedUpRecords = serde_json::from_slice(&restored.backup().await?.to_bytes()?)?;
    let writes = observer.writes.lock().unwrap();
    assert!(
        !writes.is_empty(),
        "the restored record was never published"
    );
    for doc in writes.iter() {
        let record = AppRecord::verify(doc, &identity)?;
        if withdraw {
            assert!(PaykitMember::read(&record)?.public_endpoints.is_empty());
            assert_ne!(doc, &original, "the unaccepted withdrawal was published");
        } else {
            assert_eq!(doc, &offered);
        }
    }
    let served = world.files().file(identity, &path).unwrap();
    let final_copy: SignedDoc = serde_json::from_slice(&served)?;
    assert_eq!(writes.last(), Some(&final_copy));
    assert_eq!(settled.accepted, final_copy);
    assert_eq!(settled.record, final_copy);
    assert_eq!(restored.app().record(), &final_copy);
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn on_both_services(copies: &[RestoredCopy]) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        for copy in copies {
            let Some(world) = World::start(backend)? else {
                continue;
            };
            let result = Box::pin(restore_shop(&world, *copy)).await;
            world.close().await?;
            result?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_restored_withdrawal_is_the_first_publication_seen_by_a_payer() -> TestResult {
    on_both_services(&[RestoredCopy::Missing, RestoredCopy::BeforeOffer]).await
}

#[tokio::test]
async fn test_restored_accepted_offer_is_repaired_for_a_payer() -> TestResult {
    on_both_services(&[RestoredCopy::AcceptedOnly]).await
}
