//! A buyer's wallet keeps the receipt an enrolled seller issued.
#![recursion_limit = "256"]

#[path = "receipt_publication/payer_history.rs"]
mod payer_history;

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_lib::{Asset, Decimal, PaymentAmount};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, Refusal, RequestDraft, RequestRef,
    RequestedScopes, Timer,
};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
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
    let keyring = world.keyring(&identity)?;
    let origin = origin.to_owned();
    Ok(tokio::spawn(Box::pin(async move {
        let mut signer = Signer::create(env, keyring, &origin).await?;
        signer.resume().await?;
        Ok::<_, pubky_switch_signer::SignerError>(signer)
    }))
    .await??)
}

async fn join(
    world: &World,
    mut signer: Signer,
    identity: &str,
    name: &str,
    scopes: RequestedScopes,
    store: Arc<MemoryStore>,
    rail: &Arc<FakeRail>,
) -> TestResult<(Paykit, Signer)> {
    let owner = world.identity(identity)?;
    let keys = world.app_keys(&owner, name)?;
    let auth = pubky_switch_core::enc::hash(name.as_bytes());
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let request = Asking::for_scopes_with_paths(keys, &names, &[], auth)?;
    let review = signer.review(request.request.signed(), &request.auth)?;
    let (signer, approved) = tokio::spawn(Box::pin(async move {
        let approved = signer.approve(review).await?;
        signer.claim_delivered(&approved.request).await?;
        Ok::<_, pubky_switch_signer::SignerError>((signer, approved))
    }))
    .await??;
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
    Ok((wallet, signer))
}

fn invoice(endpoint_id: &str, payload: &str) -> TestResult<RequestDraft> {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new(endpoint_id)?,
        PaymentEndpointPayload::new(payload)?,
    )]);
    Ok(RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new("invoice")?,
        RequestDestination::Endpoints(endpoints),
    ))
}

async fn paid_request(
    shop: &mut Paykit,
    mut buyer: Paykit,
    endpoint_id: &str,
    payload: &str,
) -> TestResult<(Paykit, RequestRef, [u8; 32])> {
    let draft = invoice(endpoint_id, payload)?;
    let request = shop
        .send_request(buyer.identity(), draft)
        .await?
        .request
        .ok_or("request was not sent")?;
    // The payer drives its own wallet task, just as each seller wallet does.
    // The shop's orchestration frame need not stay on the payer's native stack.
    let buyer = tokio::spawn(Box::pin(async move {
        buyer.receive(None).await?;
        let payment = buyer
            .pay_request(&request, PayOptions::default())
            .await
            .outcome?;
        let PayOutcome::Paid {
            proof_msg_id: Some(id),
            ..
        } = payment
        else {
            panic!("the wallet must report its actual proof, got {payment:?}");
        };
        Ok::<_, paykit_sdk::PaykitError>((buyer, id))
    }))
    .await??;
    let (buyer, proof_id) = buyer;
    let read = shop.receive(None).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("request missing")?;
    assert_eq!(
        sale.status,
        PayeeStatus::ProofReceived,
        "proof {proof_id} delivery {:?}; shop read {read:?}",
        buyer.app().delivery(&proof_id).await?
    );
    Ok((buyer, request, sale.record))
}

async fn with_issued_sale(scenario: impl AsyncFn(&World, IssuedSale) -> TestResult) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = match Box::pin(issued_sale(&world)).await {
            Ok(sale) => Box::pin(scenario(&world, sale)).await,
            Err(error) => Err(error),
        };
        world.close().await?;
        result?;
    }
    Ok(())
}

const SHOP: &str = "https://shop.payment.test";

struct IssuedSale {
    buyer: Paykit,
    shop: Paykit,
    rail: Arc<FakeRail>,
    request: RequestRef,
    receipt: StoredMessage,
}

async fn issued_sale(world: &World) -> TestResult<IssuedSale> {
    let payer = setup_identity(world, "buyer", "https://buyer.payment.test").await?;
    let seller = setup_identity(world, "shop", SHOP).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let (mut buyer, _) = join(
        world,
        payer,
        "buyer",
        "wallet",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let (mut shop, _) = join(
        world,
        seller,
        "shop",
        "issuer",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    let (buyer, request, key) = Box::pin(paid_request(
        &mut shop,
        buyer,
        "btc-lightning-bolt11",
        "ln-purchase",
    ))
    .await?;
    let identity = shop.identity();
    let issued = shop
        .confirm_payment(&key)
        .await?
        .ok_or("missing issued receipt")?;
    let record = shop
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing issued activity")?;
    let receipt = ActivityRecord::read(
        record.content.value().ok_or("missing issued record")?,
        &identity,
        ActivitySide::Payee,
    )?
    .receipt
    .ok_or("issued receipt missing")?;
    assert_eq!(receipt.message().msg_id, issued);
    shop.receive(None).await?;
    shop.app_mut().flush_cursor().await?;
    Ok(IssuedSale {
        buyer,
        shop,
        rail,
        request,
        receipt,
    })
}
