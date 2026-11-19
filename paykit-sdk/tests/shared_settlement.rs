//! Two independently enrolled payee apps read the receipt for a manually paid Billing Period.
#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::derive::{
    natural_key, occurrence_tag, settlement_bucket, settlement_id, settlement_tag,
};
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, OccurrenceKind, SettlementBucket, SettlementMark,
};
use paykit_lib::{Asset, Body, Decimal, PaymentAmount};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, ReceiptsOf, RequestDraft, RequestedScopes, Timer,
};
use pubky_switch_client::app::SharedRecord;
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

#[tokio::test]
async fn test_two_payee_wallets_reopen_the_same_manual_period_receipt() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(manual_period(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn manual_period(world: &World) -> TestResult {
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};

    let mut buyer_signer = Box::pin(setup_identity(
        world,
        "subscriber",
        "https://subscriber.payment.test",
    ))
    .await?;
    let mut shop_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut buyer = Box::pin(join(
        world,
        &mut buyer_signer,
        "subscriber",
        "subscriber-app",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "period-till",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Month)?, start, None)?;
    let period = PeriodRef::from(recurrence.first()?);
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
        PaymentEndpointPayload::new("ln-manual-period")?,
    )]);
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new("monthly-plan")?,
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(start + 86_400_000)
    .with_recurrence(recurrence);
    let request = Box::pin(shop.send_request(buyer.identity(), draft))
        .await?
        .request
        .ok_or("recurring request was not sent")?;
    buyer.receive(None).await?;
    Box::pin(buyer.accept_subscription(&request)).await?;
    shop.receive(None).await?;
    let (mut buyer, payment) = tokio::spawn(Box::pin(async move {
        let payment = buyer
            .pay_period(&request, &period, PayOptions::default())
            .await;
        (buyer, payment)
    }))
    .await?;
    assert!(payment.public_failures.is_empty());
    let PayOutcome::Paid { attempt_id, .. } = payment.outcome? else {
        return Err("manual payment did not settle".into());
    };
    shop.receive(None).await?;
    let period_record = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(request.request_hash))
        .and_then(|activity| {
            activity
                .periods
                .into_iter()
                .find(|item| item.billing_period == period)
        })
        .ok_or("signed period proof unavailable")?;
    assert!(period_record.paid);
    assert!(!period_record.credited);
    let receipt = Box::pin(shop.confirm_payment(&period_record.record))
        .await?
        .ok_or("missing period receipt")?;
    let issued = shop
        .app()
        .shared_record(&period_record.record)
        .await?
        .ok_or("missing shared credit")?;
    buyer.receive(None).await?;
    let received = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].attempt_id, attempt_id);
    assert_eq!(received[0].msg_id, receipt);
    let purchase = buyer.request(&request).await?;
    let payer_period = purchase
        .periods
        .iter()
        .find(|item| item.billing_period == period)
        .ok_or("payer's settled period missing")?;
    assert!(payer_period.paid && payer_period.credited);
    assert_eq!(
        payer_period.receipt.as_ref().map(|r| &r.msg_id),
        Some(&receipt)
    );
    let payments = buyer.payments().await?;
    let [payment] = payments.as_slice() else {
        return Err("expected one payer attempt".into());
    };
    assert_eq!(payment.attempt_id, attempt_id);
    assert_eq!(payment.billing_period, Some(period));
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    assert_eq!(payment.receipt.as_ref().map(|r| &r.msg_id), Some(&receipt));

    let store = Arc::new(MemoryStore::new());
    let mut second = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "second-period-till",
        AppScopes::new().request().build()?,
        store.clone(),
        &rail,
    ))
    .await?;
    assert_eq!(
        Box::pin(second.confirm_payment(&period_record.record)).await?,
        Some(receipt.clone())
    );
    let app_id = second.app_id();
    drop(second);
    let mut reopened = Box::pin(Paykit::load(
        world.app_env(store),
        Default::default(),
        app_id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    reopened.receive(None).await?;
    assert_eq!(
        Box::pin(reopened.confirm_payment(&period_record.record)).await?,
        Some(receipt.clone())
    );
    let current = reopened
        .app()
        .shared_record(&period_record.record)
        .await?
        .ok_or("reopened period credit missing")?;
    assert_eq!(current.version, issued.version);
    assert_eq!(current.writer, issued.writer);
    Box::pin(check_period_mark(
        &reopened,
        &current,
        &request.request_hash,
        period,
    ))
    .await?;
    let credited = reopened
        .payee_activity()
        .await?
        .into_iter()
        .flat_map(|activity| activity.periods)
        .filter(|item| item.credited)
        .count();
    assert_eq!(credited, 1);
    buyer.receive(None).await?;
    let received = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].msg_id, receipt);
    assert_eq!(rail.all_executions().values().copied().sum::<u64>(), 1);
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    Ok(())
}

async fn check_period_mark(
    shop: &Paykit,
    record: &SharedRecord,
    request_hash: &[u8; 32],
    period: PeriodRef,
) -> TestResult {
    let naming = shop.app().naming_key();
    let activity = ActivityRecord::bind(
        record.content.value().ok_or("missing paid period")?,
        &shop.identity(),
        ActivitySide::Payee,
        naming,
        &record.key,
        None,
    )?;
    let signed = activity.proof.as_ref().ok_or("missing period proof")?;
    let Body::PaymentProof(proof) = signed.body() else {
        return Err("expected a payment proof".into());
    };
    let id = settlement_id(&proof.proof).ok_or("missing method settlement ID")?;
    let tag = settlement_tag(naming, &id);
    let number = settlement_bucket(&tag);
    let key = SettlementBucket::record_key(number, naming)?;
    let shared = shop
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing shared settlement bucket")?;
    let bucket = SettlementBucket::bind(
        shared
            .content
            .value()
            .ok_or("unreadable settlement bucket")?,
        number,
        naming,
        &key,
    )?;
    let natural = natural_key::period_record(&shop.identity(), request_hash, period.start_secs());
    let expected = SettlementMark::new(
        &tag,
        OccurrenceKind::BillingPeriod,
        occurrence_tag(naming, &natural),
    );
    assert_eq!(bucket.marks(), &[expected]);
    Ok(())
}
