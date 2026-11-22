//! The ordinary wallet journeys, end to end through the SDK, on both backends: the client's
//! scripted switches and the Switch service on PostgreSQL.
//!
//! Each identity has its own switch and its own signer, which approves each Paykit app it
//! joins. Each test runs its journey on both backends, each on a fresh world, and names the
//! backend in any failure. The service backend needs `PUBKY_SWITCH_TEST_DATABASE_URL`.
//! Without it the service run is skipped, as in the other service tests, unless
//! `PUBKY_SWITCH_REQUIRE_DB` is set, which CI sets; with it, a database that can't be reached
//! fails the test.
#![recursion_limit = "256"]

#[path = "journeys/connect.rs"]
mod connect;
#[path = "journeys/one_time.rs"]
mod one_time;
#[path = "journeys/private_list.rs"]
mod private_list;
#[path = "journeys/public_endpoint.rs"]
mod public_endpoint;

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::{Asset, Decimal, PaymentAmount};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, AttemptId, PayOutcome, PayeeActivity, PayeeStatus, Paykit, PaykitConfig, ReceiptsOf,
    RequestDraft, RequestedScopes, Stage, Timer,
};
use pubky_switch_client::app::AppEnv;
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::transport::{BoxFuture, Request, Response, Transport, TransportError};
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{Protocols, Signer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The host's timer: a real sleep, as on a device.
struct HostTimer;

impl Timer for HostTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

/// The service handles each request on a task of its own, as at its HTTP boundary, so a
/// wallet's call never runs the service's handling on the wallet's own stack.
struct ServiceTask(Arc<dyn Transport>);

impl Transport for ServiceTask {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::spawn(async move { service.send(request).await }).await {
                Ok(answer) => answer,
                Err(error) => std::panic::resume_unwind(error.into_panic()),
            }
        })
    }
}

fn on_backend(world: &World, transport: Arc<dyn Transport>) -> Arc<dyn Transport> {
    match world.backend() {
        Backend::Service => Arc::new(ServiceTask(transport)),
        _ => transport,
    }
}

/// Run `journey` on a fresh world of each backend, closing each world after its run, and
/// fail with every backend's failure, by name.
async fn on_both_backends<F>(journey: F) -> TestResult
where
    F: AsyncFn(&World) -> TestResult,
{
    let mut failures = Vec::new();
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(journey(&world)).await;
        let closed = world.close().await;
        if let Err(error) = result {
            failures.push(format!("{backend:?}: {error}"));
        }
        closed?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

/// An identity with its own switch and its own signer.
struct Owner {
    name: String,
    signer: Signer,
}

impl Owner {
    /// Create `name`'s identity, its switch at an origin of its own, and its signer.
    async fn enroll(world: &World, name: &str) -> TestResult<Self> {
        let identity = world.identity(name)?;
        let origin = format!("https://{name}.journey.test");
        Box::pin(world.add_switch(&origin, &[&identity])).await?;
        let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
        env.transport = on_backend(world, env.transport);
        env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
        let mut signer = Box::pin(Signer::create(env, world.keyring(&identity)?, &origin)).await?;
        Box::pin(signer.resume()).await?;
        Ok(Self {
            name: name.to_owned(),
            signer,
        })
    }

    /// The identity's public key.
    fn identity(&self, world: &World) -> TestResult<[u8; 32]> {
        Ok(world.identity(&self.name)?.public())
    }

    /// Join a Paykit app `app` of this identity: the signer approves its scopes, and the
    /// app joins with its own store and reads its inbox once.
    async fn join(
        &mut self,
        world: &World,
        app: &str,
        scopes: RequestedScopes,
        config: PaykitConfig,
        store: Arc<MemoryStore>,
        rail: &Arc<FakeRail>,
    ) -> TestResult<Paykit> {
        let identity = world.identity(&self.name)?;
        let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
        let asking = Asking::for_scopes_with_paths(
            world.app_keys(&identity, app)?,
            &names,
            &[],
            pubky_switch_core::enc::hash(app.as_bytes()),
        )?;
        let review = self.signer.review(asking.request.signed(), &asking.auth)?;
        let approved = Box::pin(self.signer.approve(review)).await?;
        Box::pin(self.signer.claim_delivered(&approved.request)).await?;
        world.advance_ms(1_000);
        let mut wallet = Box::pin(Paykit::join(
            app_env(world, store),
            config,
            asking.join_request(identity.public(), &approved)?,
            rail.clone(),
            Arc::new(HostTimer),
        ))
        .await?;
        Box::pin(wallet.receive(None)).await?;
        Ok(wallet)
    }
}

/// The environment of an app on `world`, with its own store.
fn app_env(world: &World, store: Arc<MemoryStore>) -> AppEnv {
    let mut env = world.app_env(store);
    env.transport = on_backend(world, env.transport);
    env
}

/// A payer's wallet and a seller's shop, each of an identity of its own, admitting each
/// other, on one rail.
struct Checkout {
    payer: Owner,
    wallet: Paykit,
    shop: Paykit,
    rail: Arc<FakeRail>,
}

/// Enroll the payer and the seller, join the payer's wallet and the seller's shop, with
/// the shop on `shop_config`, and connect them.
async fn checkout(world: &World, shop_config: PaykitConfig) -> TestResult<Checkout> {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut payer = Box::pin(Owner::enroll(world, "payer")).await?;
    let mut seller = Box::pin(Owner::enroll(world, "seller")).await?;
    let mut wallet = Box::pin(payer.join(
        world,
        "wallet",
        AppScopes::new().pay().manage_contacts().build()?,
        PaykitConfig::default(),
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(seller.join(
        world,
        "shop",
        AppScopes::new().request().manage_contacts().build()?,
        shop_config,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    Box::pin(connect(&mut wallet, &mut shop)).await?;
    Ok(Checkout {
        payer,
        wallet,
        shop,
        rail,
    })
}

/// Both sides admit each other: each invites the other, as a payer does at a checkout.
async fn connect(a: &mut Paykit, b: &mut Paykit) -> TestResult {
    Box::pin(a.app_mut().invite(&b.identity())).await?;
    Box::pin(b.app_mut().invite(&a.identity())).await?;
    Ok(())
}

/// What a payment is expected to be, on both sides.
struct Expected {
    request_hash: Option<[u8; 32]>,
    amount: PaymentAmount,
    endpoint_id: PaymentEndpointIdentifier,
}

/// After the payer's `outcome`: the method was called once; the payer's journal holds the
/// payment, finished; the seller holds the payer's proof, verifies it, confirms it and shows
/// it receipted; and the payer then holds exactly that receipt. Returns the receipt's
/// `msg_id`.
async fn paid_and_receipted(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    rail: &FakeRail,
    outcome: PayOutcome,
    expected: &Expected,
) -> TestResult<String> {
    let PayOutcome::Paid {
        attempt_id,
        proof_msg_id: Some(_),
        ..
    } = outcome
    else {
        return Err(format!("not paid with a proof: {outcome:?}").into());
    };
    assert_eq!(rail.executions(&attempt_id), 1);

    // The payer's view: its own payment, finished.
    let payments = Box::pin(wallet.payments()).await?;
    let payment = payments
        .iter()
        .find(|payment| payment.attempt_id == attempt_id)
        .ok_or("the payer's journal lacks the payment")?;
    assert_eq!(payment.payee, shop.identity());
    assert_eq!(payment.request_hash, expected.request_hash);
    assert_eq!(payment.amount, expected.amount);
    assert_eq!(payment.endpoint_id, expected.endpoint_id);
    assert_eq!(payment.stage, Stage::Finished, "{payment:?}");

    // The seller's view: the proof, verified, then the receipt.
    Box::pin(shop.receive(None)).await?;
    let sale = Box::pin(sale_of(shop, attempt_id)).await?;
    assert_eq!(sale.payer, wallet.identity());
    assert_eq!(sale.request_hash, expected.request_hash);
    assert_eq!(sale.amount.as_ref(), Some(&expected.amount));
    assert_eq!(sale.status, PayeeStatus::ProofReceived, "{sale:?}");
    assert!(Box::pin(shop.verify_payment(&sale.record)).await?.is_some());
    let receipt = Box::pin(shop.confirm_payment(&sale.record))
        .await?
        .ok_or("the seller could not confirm the payment")?;
    let sale = Box::pin(sale_of(shop, attempt_id)).await?;
    assert_eq!(sale.status, PayeeStatus::Receipted, "{sale:?}");

    // The payer's view again: the seller's receipt of this payment.
    Box::pin(wallet.receive(None)).await?;
    let receipts = Box::pin(wallet.receipts(ReceiptsOf::Payee(shop.identity()))).await?;
    let held: Vec<_> = receipts
        .iter()
        .filter(|receipt| receipt.attempt_id == attempt_id)
        .collect();
    assert_eq!(held.len(), 1, "{receipts:?}");
    assert_eq!(held[0].msg_id, receipt);
    assert_eq!(held[0].request_hash, expected.request_hash);
    Ok(receipt)
}

/// The seller's activity record of the payment `attempt_id`.
async fn sale_of(shop: &Paykit, attempt_id: AttemptId) -> TestResult<PayeeActivity> {
    let sales = Box::pin(shop.payee_activity()).await?;
    let mut of_attempt = sales
        .iter()
        .filter(|sale| sale.attempt_id == Some(attempt_id));
    let sale = of_attempt
        .next()
        .ok_or_else(|| format!("the seller has no record of the payment: {sales:?}"))?;
    assert!(of_attempt.next().is_none(), "{sales:?}");
    Ok(sale.clone())
}

fn btc(value: &str) -> TestResult<PaymentAmount> {
    Ok(PaymentAmount {
        asset: Asset::new("btc")?,
        value: Decimal::new(value)?,
    })
}

fn endpoint(id: &str) -> TestResult<PaymentEndpointIdentifier> {
    Ok(PaymentEndpointIdentifier::new(id)?)
}

fn payload(text: &str) -> TestResult<PaymentEndpointPayload> {
    Ok(PaymentEndpointPayload::new(text)?)
}

/// A one-time request for `value` BTC to a Lightning invoice, under `reference`.
fn invoice(value: &str, reference: &str, invoice: &str) -> TestResult<RequestDraft> {
    let endpoints = [(endpoint("btc-lightning-bolt11")?, payload(invoice)?)]
        .into_iter()
        .collect();
    Ok(RequestDraft::new(
        btc(value)?,
        PaymentReference::new(reference)?,
        RequestDestination::Endpoints(endpoints),
    ))
}
