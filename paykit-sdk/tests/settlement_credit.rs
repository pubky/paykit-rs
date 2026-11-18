//! Independent tills confirm a shared invoice settlement through the ordinary wallet.
use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, occurrence_tag, settlement_bucket, settlement_tag};
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::records::{OccurrenceKind, SettlementBucket, SettlementMark};
use paykit_lib::{Asset, Decimal, Label, PaymentAmount};
use paykit_sdk::rail::{FakeRail, Rail};
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, ReceiptDetails, ReceiptsOf,
    RequestDraft, RequestRef, RequestedScopes, Timer,
};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{Protocols, Signer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct TestTimer;
impl Timer for TestTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

async fn setup_identity(world: &World, name: &str, origin: &str) -> TestResult<Signer> {
    let identity = world.identity(name)?;
    world.add_switch(origin, &[&identity]).await?;
    let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    let mut signer = Signer::create(env, world.keyring(&identity)?, origin).await?;
    signer.resume().await?;
    Ok(signer)
}

async fn join(
    world: &World,
    signer: &mut Signer,
    identity: &str,
    name: &str,
    scopes: RequestedScopes,
    store: Arc<MemoryStore>,
    rail: &Arc<FakeRail>,
) -> TestResult<Paykit> {
    let owner = world.identity(identity)?;
    let keys = world.app_keys(&owner, name)?;
    let auth = pubky_switch_core::enc::hash(name.as_bytes());
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let request = Asking::for_scopes_with_paths(keys, &names, &[], auth)?;
    let approved = signer
        .approve(signer.review(request.request.signed(), &request.auth)?)
        .await?;
    signer.claim_delivered(&approved.request).await?;
    let join = request.join_request(owner.public(), &approved)?;
    world.advance_ms(1000);
    let mut wallet = Paykit::join(
        world.app_env(store),
        Default::default(),
        join,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    wallet.receive(None).await?;
    Ok(wallet)
}

async fn paid_request(
    shop: &mut Paykit,
    buyer: &mut Paykit,
    invoice: &str,
) -> TestResult<(RequestRef, [u8; 32])> {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
        PaymentEndpointPayload::new(invoice)?,
    )]);
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new("invoice")?,
        RequestDestination::Endpoints(endpoints),
    );
    let request = shop
        .send_request(buyer.identity(), draft)
        .await?
        .request
        .ok_or("request was not sent")?;
    buyer.receive(None).await?;
    assert!(matches!(
        buyer
            .pay_request(&request, PayOptions::default())
            .await
            .outcome?,
        PayOutcome::Paid { .. }
    ));
    shop.receive(None).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("request missing")?;
    assert_eq!(sale.status, PayeeStatus::ProofReceived);
    Ok((request, sale.record))
}

#[tokio::test]
async fn test_two_tills_confirm_one_paid_request() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(competing_tills(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn competing_tills(world: &World) -> TestResult {
    let mut first = setup_identity(world, "buyer", "https://buyer.payment.test").await?;
    let mut second =
        setup_identity(world, "other-buyer", "https://other-buyer.payment.test").await?;
    let mut shop = setup_identity(world, "shop", "https://shop.payment.test").await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut buyer = join(
        world,
        &mut first,
        "buyer",
        "first-buyer",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let mut other = join(
        world,
        &mut second,
        "other-buyer",
        "second-buyer",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let till_store = Arc::new(MemoryStore::new());
    let mut till = join(
        world,
        &mut shop,
        "shop",
        "plain-till",
        AppScopes::new().request().manage_contacts().build()?,
        till_store.clone(),
        &rail,
    )
    .await?;
    let labelled_store = Arc::new(MemoryStore::new());
    let mut labelled = join(
        world,
        &mut shop,
        "shop",
        "labelled-till",
        AppScopes::new()
            .request_label(&Label::parse("shop1")?)
            .manage_contacts()
            .build()?,
        labelled_store.clone(),
        &rail,
    )
    .await?;
    for wallet in [&mut buyer, &mut other] {
        wallet.app_mut().invite(&till.identity()).await?;
        till.app_mut().invite(&wallet.identity()).await?;
    }
    let before_credit = till.backup().await?;
    // Both authorized tills see the same legitimate purchase in the shop's labelled activity.
    let (winner, first_record) =
        Box::pin(paid_request(&mut labelled, &mut buyer, "ln-one-invoice")).await?;
    till.receive(None).await?;
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);
    assert!(till.verify_payment(&first_record).await?.is_some());
    assert!(labelled.verify_payment(&first_record).await?.is_some());
    let (first_receipt, second_receipt) = tokio::join!(
        till.confirm_payment(&first_record),
        labelled.confirm_payment(&first_record)
    );
    let receipt = first_receipt?.ok_or("first till could not confirm the paid request")?;
    assert_eq!(second_receipt?, Some(receipt.clone()));
    buyer.receive(None).await?;
    let receipts = buyer.receipts(ReceiptsOf::Request(winner)).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, receipt);
    let first_id = till.app_id();
    let second_id = labelled.app_id();
    drop((till, labelled));
    let mut reopened = Paykit::load(
        world.app_env(till_store.clone()),
        Default::default(),
        first_id,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    let mut other_till = Paykit::load(
        world.app_env(labelled_store),
        Default::default(),
        second_id,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    reopened.receive(None).await?;
    other_till.receive(None).await?;
    assert_eq!(
        reopened.confirm_payment(&first_record).await?,
        Some(receipt.clone())
    );
    assert_eq!(
        other_till.confirm_payment(&first_record).await?,
        Some(receipt.clone())
    );
    let checks = rail.proof_checks();
    let id = rail
        .settlement_id(&checks[0].proof)
        .ok_or("verified invoice has no settlement")?;
    for reader in [&reopened, &other_till] {
        let naming = reader.app().naming_key();
        let tag = settlement_tag(naming, &id);
        let number = settlement_bucket(&tag);
        let key = SettlementBucket::record_key(number, naming)?;
        let records = reader
            .app()
            .records_of(Collection::Settlements.name())
            .await?;
        let bucket = records
            .iter()
            .find(|r| r.key == key)
            .ok_or("settlement record missing")?;
        assert_eq!(bucket.policy, Collection::Settlements.fixed_policy());
        let view = SettlementBucket::bind(
            bucket.content.value().ok_or("bucket unreadable")?,
            number,
            naming,
            &key,
        )?;
        let occurrence = occurrence_tag(
            naming,
            &natural_key::request_record(&reader.identity(), &winner.request_hash),
        );
        let mark = SettlementMark::new(&tag, OccurrenceKind::OneTimeRequest, occurrence);
        assert_eq!(view.marks(), &[mark]);
        let retained = format!(
            "sm/{}/{}",
            pubky_switch_core::enc::b64(&key),
            pubky_switch_core::enc::b64(&mark.to_bytes())
        );
        assert!(reader
            .app()
            .protocol_get("paykit", retained.as_bytes())
            .await?
            .is_some());
    }
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);

    let (second, second_record) = Box::pin(paid_request(
        &mut reopened,
        &mut other,
        "ln-independent-paid",
    ))
    .await?;
    let second_receipt = reopened
        .confirm_payment(&second_record)
        .await?
        .ok_or("second receipt missing")?;
    other.receive(None).await?;
    let receipts = other.receipts(ReceiptsOf::Request(second)).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, second_receipt);
    assert_ne!(receipts[0].msg_id, receipt);
    assert_eq!(rail.all_executions().values().sum::<u64>(), 2);

    let (independent, third_record) = Box::pin(paid_request(
        &mut other_till,
        &mut other,
        "ln-independent-invoice",
    ))
    .await?;
    let stored = other_till
        .confirm_payment_with(&third_record, ReceiptDetails::new().store_only())
        .await?
        .ok_or("independent credit missing")?;
    assert!(other_till.app().delivery(&stored).await?.is_none());
    other.receive(None).await?;
    assert!(other
        .receipts(ReceiptsOf::Request(independent))
        .await?
        .is_empty());
    let backup = other_till.backup().await?;
    drop(other_till);
    let mut restored = Paykit::restore(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        &backup,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    restored.receive(None).await?;
    assert_eq!(
        restored.confirm_payment(&third_record).await?,
        Some(stored.clone())
    );
    assert!(restored.app().delivery(&stored).await?.is_none());
    other.receive(None).await?;
    assert!(other
        .receipts(ReceiptsOf::Request(independent))
        .await?
        .is_empty());
    assert_eq!(rail.all_executions().values().sum::<u64>(), 3);
    // Selecting a backup predating these credits keeps the marks this installation
    // already saw. A new store learns them from its own authenticated read.
    reopened.receive(None).await?;
    let marks = reopened.app().protocol_scan("paykit", b"sm/").await?;
    assert_eq!(marks.len(), 3);
    drop(reopened);
    let in_place = Paykit::restore(
        world.app_env(till_store),
        Default::default(),
        &before_credit,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert_eq!(in_place.app().protocol_scan("paykit", b"sm/").await?, marks);
    drop(in_place);
    let mut fresh = Paykit::restore(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        &before_credit,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert!(fresh
        .app()
        .protocol_scan("paykit", b"sm/")
        .await?
        .is_empty());
    fresh.receive(None).await?;
    assert_eq!(fresh.app().protocol_scan("paykit", b"sm/").await?, marks);
    Ok(())
}
