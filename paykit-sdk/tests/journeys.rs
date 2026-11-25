//! The ordinary wallet journeys, end to end through the SDK, on both backends: the client's
//! scripted switches and the Dispatch service on PostgreSQL.
//!
//! Each identity has its own Dispatch and its own signer, which approves each Paykit app it
//! joins. Each test runs its journey on both backends, each on a fresh world, and names the
//! backend in any failure. The service backend needs `PUBKY_SWITCH_TEST_DATABASE_URL`.
//! Without it the service run is skipped, as in the other service tests, unless
//! `PUBKY_SWITCH_REQUIRE_DB` is set, which CI sets; with it, a database that can't be reached
//! fails the test.
#![recursion_limit = "256"]

#[path = "journeys/allowances.rs"]
mod allowances;
#[path = "journeys/connect.rs"]
mod connect;
#[path = "journeys/one_time.rs"]
mod one_time;
#[path = "journeys/periods.rs"]
mod periods;
#[path = "journeys/priced.rs"]
mod priced;
#[path = "journeys/private_list.rs"]
mod private_list;
#[path = "journeys/provider_loss.rs"]
mod provider_loss;
#[path = "journeys/public_endpoint.rs"]
mod public_endpoint;
#[path = "journeys/restore.rs"]
mod restore;

use std::any::Any;
use std::error::Error;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
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
use pubky_dispatch_client::app::AppEnv;
use pubky_dispatch_client::store::MemoryStore;
use pubky_dispatch_client::transport::{BoxFuture, Request, Response, Transport, TransportError};
use pubky_dispatch_harness::{parties::Asking, Backend, World};
use pubky_dispatch_signer::{Protocols, Signer};

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
/// fail with every backend's failure, by name. A panic, as a failed assertion raises, is
/// caught as that backend's failure, so the other backend still runs and reports.
async fn on_both_backends<F>(journey: F) -> TestResult
where
    F: AsyncFn(&World) -> TestResult,
{
    let mut failures = Vec::new();
    for backend in [Backend::Scripted, Backend::Service] {
        let world = match std::panic::catch_unwind(|| World::start(backend)) {
            Ok(Ok(Some(world))) => world,
            Ok(Ok(None)) => continue,
            Ok(Err(error)) => {
                failures.push(format!("{backend:?}: starting the world: {error}"));
                continue;
            }
            Err(panic) => {
                failures.push(format!(
                    "{backend:?}: starting the world: {}",
                    message(&*panic)
                ));
                continue;
            }
        };
        let result = Caught(Box::pin(journey(&world))).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("{backend:?}: {error}")),
            Err(panic) => failures.push(format!("{backend:?}: panicked: {panic}")),
        }
        if let Err(error) = world.close().await {
            failures.push(format!("{backend:?}: closing the world: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

#[tokio::test]
async fn test_journeys_report_a_failed_assertion_on_each_backend() -> TestResult {
    let runs = AtomicUsize::new(0);
    let result = on_both_backends(async |world: &World| {
        runs.fetch_add(1, Ordering::SeqCst);
        assert!(world.backend() != Backend::Scripted, "a failed assertion");
        Err("a failure the journey returned".into())
    })
    .await;
    let failure = result.err().ok_or("a failing journey passed")?.to_string();
    // Scripted fails its assertion, and the run goes on: the service backend, when it is
    // configured, still runs and reports the failure it returned.
    assert!(failure.contains("Scripted: panicked: "), "{failure}");
    assert!(failure.contains("a failed assertion"), "{failure}");
    match runs.load(Ordering::SeqCst) {
        1 => assert!(!failure.contains("Service"), "{failure}"),
        2 => assert!(
            failure.contains("Service: a failure the journey returned"),
            "{failure}"
        ),
        other => return Err(format!("the journey ran {other} times").into()),
    }
    Ok(())
}

/// A future whose panic is caught and returned as its message.
struct Caught<F>(Pin<Box<F>>);

impl<F: Future> Future for Caught<F> {
    type Output = Result<F::Output, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(panic) => Poll::Ready(Err(message(&*panic))),
        }
    }
}

/// The message a panic carried.
fn message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "a panic without a message".to_owned())
}

/// An identity with its own Dispatch and its own signer.
struct Owner {
    name: String,
    signer: Signer,
}

impl Owner {
    /// Create `name`'s identity, its Dispatch at an origin of its own, and its signer.
    async fn enroll(world: &World, name: &str) -> TestResult<Self> {
        let identity = world.identity(name)?;
        let origin = format!("https://{name}.journey.test");
        Box::pin(world.add_dispatch(&origin, &[&identity])).await?;
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
        let mut wallet =
            Box::pin(self.join_unlisted(world, app, scopes, config, store, rail)).await?;
        Box::pin(finish_listing(&mut wallet)).await?;
        Ok(wallet)
    }

    /// [`Self::join`], before the app reads anything.
    async fn join_unlisted(
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
            pubky_dispatch_core::enc::hash(app.as_bytes()),
        )?;
        let review = self.signer.review(asking.request.signed(), &asking.auth)?;
        let approved = Box::pin(self.signer.approve(review)).await?;
        Box::pin(self.signer.claim_delivered(&approved.request)).await?;
        world.advance_ms(1_000);
        Ok(Box::pin(Paykit::join(
            app_env(world, store),
            config,
            asking.join_request(identity.public(), &approved)?,
            rail.clone(),
            Arc::new(HostTimer),
        ))
        .await?)
    }
}

/// Read the inbox until the app's first listing is done, as a joined wallet does.
async fn finish_listing(wallet: &mut Paykit) -> TestResult {
    for _ in 0..8 {
        let read = Box::pin(wallet.receive(None)).await?;
        if !read.report.more && read.records_unlisted.is_none() {
            assert!(!wallet.app().listing_owed().await?);
            return Ok(());
        }
    }
    Err("the wallet's first listing did not finish".into())
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

/// Both sides admit each other, each allowing the other to connect, as a payer and a seller
/// do at a checkout.
async fn connect(a: &mut Paykit, b: &mut Paykit) -> TestResult {
    let (a_id, b_id) = (a.identity(), b.identity());
    Box::pin(a.allow_connect(&b_id)).await?;
    Box::pin(b.allow_connect(&a_id)).await?;
    Ok(())
}

/// What a payment is expected to be, on both sides.
struct Expected {
    request_hash: Option<[u8; 32]>,
    amount: PaymentAmount,
    endpoint_id: PaymentEndpointIdentifier,
    /// The amount the method transferred: the requested one unless a rate priced it.
    paid: PaymentAmount,
    /// The rate that priced the payment, if one did.
    rate: Option<&'static str>,
    /// How many selections the method had made before the payment.
    selections: usize,
}

impl Expected {
    /// A payment of `amount` to `endpoint_id`, of the request `request_hash` if any, about
    /// to be made on `rail`: made before the payment, it notes the method's selections so
    /// far.
    fn new(
        rail: &FakeRail,
        request_hash: Option<[u8; 32]>,
        amount: PaymentAmount,
        endpoint_id: PaymentEndpointIdentifier,
    ) -> Self {
        Self {
            request_hash,
            paid: amount.clone(),
            amount,
            endpoint_id,
            rate: None,
            selections: rail.selections().len(),
        }
    }

    /// The same payment priced at `rate`, the method transferring `paid`.
    fn at_rate(self, rate: &'static str, paid: PaymentAmount) -> Self {
        Self {
            rate: Some(rate),
            paid,
            ..self
        }
    }
}

/// After the payer's `outcome`: the method was handed one new selection, including the
/// expected endpoint, and called once; the payer's journal holds the
/// payment, finished; the seller holds the payer's proof, verifies it, confirms it and shows
/// it receipted; and the payer then holds exactly that receipt, in its receipts and in its
/// journal's entry. Returns the receipt's `msg_id`.
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
    let selections = rail.selections();
    assert_eq!(selections.len(), expected.selections + 1, "{selections:?}");
    let (candidates, _) = selections
        .last()
        .ok_or("the method was handed no endpoint")?;
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.endpoint_id == expected.endpoint_id),
        "{candidates:?}"
    );

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
    assert_eq!(payment.paid.as_ref(), Some(&expected.paid), "{payment:?}");
    assert_eq!(
        payment.rate.as_ref().map(|rate| rate.value().as_str()),
        expected.rate
    );
    assert_eq!(rail.paid_amount(&attempt_id).as_ref(), Some(&expected.paid));
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
    let payments = Box::pin(wallet.payments()).await?;
    let journal = payments
        .iter()
        .find(|payment| payment.attempt_id == attempt_id)
        .and_then(|payment| payment.receipt.as_ref())
        .ok_or("the payer's journal shows no receipt")?;
    assert_eq!(journal.msg_id, receipt);
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
