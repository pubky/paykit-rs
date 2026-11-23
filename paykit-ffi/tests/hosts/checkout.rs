//! An enrolled shop for exercising the native host clients on the local Switch harness.
use crate::{
    Amount, EndpointSupport, HostRail, MethodDeclaration, PaymentMethod, RequestId, RuntimeTimer,
    WalletError, WalletHost, WalletStore,
};
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft, RequestRef};
use pubky_switch_client::{
    store::MemoryStore,
    transport::{BoxFuture, FirstSend, Request, Response, Transport, TransportError},
};
use pubky_switch_core::{
    records::{AppRequest, AuthRequest},
    sig::SignedDoc,
};
use pubky_switch_harness::{parties::Asking, World};
use pubky_switch_signer::Signer;
use std::{collections::BTreeMap, error::Error, sync::Arc, time::Duration};
use tokio::sync::Mutex;

use crate::fixture_signer as signer;

// Poll the in-process service on its own task, as a remote service would be. The host
// executor still runs the wallet and its actual protocol verifiers.
struct CheckoutService(Arc<dyn Transport>);

impl Transport for CheckoutService {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        let service = self.0.clone();
        Box::pin(async move {
            tokio::spawn(async move { service.send(request).await })
                .await
                .expect("local checkout service stopped")
        })
    }

    fn send_before<'a>(
        &'a self,
        request: Request,
        first: &'a FirstSend,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        self.0.send_before(request, first)
    }
}

/// A local seller and independent approving signer, available with the test-wallet feature.
#[derive(uniffi::Object)]
pub struct CheckoutFixture {
    world: Mutex<Option<World>>,
    host: Arc<WalletHost>,
    method: Method,
    signer: Mutex<Signer>,
    seller: Mutex<Paykit>,
    buyer: [u8; 32],
    payee: [u8; 32],
}

impl CheckoutFixture {
    async fn open(
        store: Arc<dyn WalletStore>,
        method: Method,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        let world = crate::fixture_service::start()?;
        let buyer = world.identity("buyer")?.public();
        let owner = world.identity("shop")?;
        let signer = Box::pin(signer::serving(&world, "buyer")).await?;
        let mut issuer = Box::pin(signer::serving(&world, "shop")).await?;
        let rail = Arc::new(device_rail(&method));
        let host = Arc::new(WalletHost {
            clock: world.clock().clone(),
            transport: Arc::new(CheckoutService(world.transport())),
            home: world.files().clone(),
            writer: world.files().clone(),
            store,
            rail: rail.clone(),
            timer: Arc::new(RuntimeTimer),
        });
        let scopes = AppScopes::new().request().build()?;
        let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
        let asking = Asking::for_scopes(world.app_keys(&owner, "wallet")?, &names)?;
        let review = issuer.review(asking.request.signed(), &asking.auth)?;
        let approval = Box::pin(issuer.approve(review)).await?;
        let mut seller = Box::pin(Paykit::join(
            world.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            asking.join_request(owner.public(), &approval)?,
            rail,
            Arc::new(RuntimeTimer),
        ))
        .await?;
        issuer.claim_delivered(&approval.request).await?;
        Box::pin(seller.allow_connect(&buyer)).await?;
        Ok(Arc::new(Self {
            world: Mutex::new(Some(world)),
            host,
            method,
            signer: Mutex::new(signer),
            seller: Mutex::new(seller),
            buyer,
            payee: owner.public(),
        }))
    }

    async fn approve_request(
        &self,
        signed: &[u8],
        secret: [u8; 32],
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let document: SignedDoc = serde_json::from_slice(signed)?;
        let request = AppRequest::verify(&document)?;
        let auth = AuthRequest {
            client_id: request.body().client_id.clone(),
            client_key: request.body().client_key,
            caps: request.body().caps.clone(),
            secret,
        };
        let mut signer = self.signer.lock().await;
        let reviewed = signer.review(&document, &auth)?;
        let approved = Box::pin(signer.approve(reviewed)).await?;
        Ok(approved.claim)
    }
}

// The harness signer and service changes use local futures. Keep them on a native worker
// while Swift and Kotlin drive the regular Send wallet API and their own method callbacks.
async fn harness<T: Send + 'static>(
    operation: impl AsyncFnOnce() -> Result<T, WalletError> + Send + 'static,
) -> Result<T, WalletError> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || runtime.block_on(operation()))
        .await
        .map_err(|_| WalletError::invalid("local checkout worker"))?
}

/// How long the test wallets wait for the host's `pay` before recording it as possibly sent.
const TEST_PAY_LIMIT: Duration = Duration::from_secs(2);

/// Each test device keeps its own record of payments in flight, as separate devices do.
fn device_rail(method: &Method) -> HostRail {
    HostRail::new(
        method.method.clone(),
        method.declaration.clone(),
        method.endpoints.clone(),
    )
    .with_pay_limit(TEST_PAY_LIMIT)
    .with_own_flights()
}

/// The host's payment method and the data it declares with it.
struct Method {
    method: Arc<dyn PaymentMethod>,
    declaration: MethodDeclaration,
    endpoints: Vec<(PaymentEndpointIdentifier, Option<u32>)>,
}

fn invoice() -> paykit_lib::Result<RequestDraft> {
    Ok(RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("7")?,
        },
        PaymentReference::new("checkout-7")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new("lnbc1checkout")?,
        )])),
    ))
}

/// Enroll a local shop and use the supplied foreign store and method on the buyer's wallet,
/// with the declaration and endpoints the host would give `WalletHost`.
#[uniffi::export]
pub async fn enrolled_test_checkout(
    store: Arc<dyn WalletStore>,
    method: Arc<dyn PaymentMethod>,
    declaration: MethodDeclaration,
    endpoints: Vec<EndpointSupport>,
) -> Result<Arc<CheckoutFixture>, WalletError> {
    let method = Method {
        method,
        declaration,
        endpoints: crate::environment::supported_endpoints(endpoints)?,
    };
    crate::runtime::run(async move {
        harness(async move || {
            CheckoutFixture::open(store, method)
                .await
                .map_err(|_| WalletError::invalid("test checkout setup"))
        })
        .await
    })
    .await
}

#[uniffi::export]
impl CheckoutFixture {
    /// Use the original SDK wallet and host callbacks with the local test services.
    pub fn host(&self) -> Arc<WalletHost> {
        self.host.clone()
    }
    /// Open a second device's store against the same enrolled services and payment account.
    pub fn host_for_store(&self, store: Arc<dyn WalletStore>) -> Arc<WalletHost> {
        Arc::new(WalletHost {
            clock: self.host.clock.clone(),
            transport: self.host.transport.clone(),
            home: self.host.home.clone(),
            writer: self.host.writer.clone(),
            // Another device: the host's calls in progress on this one are not its own.
            rail: Arc::new(device_rail(&self.method)),
            timer: self.host.timer.clone(),
            store,
        })
    }

    /// Let the current device rotate its receive key on its next ordinary foreground read.
    pub async fn pass_rotation_interval(self: Arc<Self>) -> Result<(), WalletError> {
        crate::runtime::run(async move {
            let world = self.world.lock().await;
            let world = world
                .as_ref()
                .ok_or(WalletError::invalid("test service closed"))?;
            world.advance_ms(pubky_switch_client::app::DEFAULT_ROTATION_MS + 1);
            Ok(())
        })
        .await
    }
    /// The identity that signs the buyer's enrollment.
    pub fn identity(&self) -> Vec<u8> {
        self.buyer.into()
    }
    /// The test services' clock now, in Unix milliseconds, which a host's test payment
    /// method dates its payments by.
    pub fn now_ms(&self) -> u64 {
        self.host.clock.now_ms()
    }
    /// The identity of the enrolled shop.
    pub fn payee(&self) -> Vec<u8> {
        self.payee.into()
    }
    /// The SDK's pre-loss acceptance and confirmed withdrawal, projected for the host enum check.
    pub fn allowance_recovery_notice_kinds(&self) -> Vec<crate::WalletNoticeKind> {
        use paykit_sdk::NoticeKind::{AllowanceAcceptanceBeforeLoss, AllowanceWithdrawalConfirmed};
        vec![
            AllowanceAcceptanceBeforeLoss.into(),
            AllowanceWithdrawalConfirmed.into(),
        ]
    }
    /// Approve the actual host request using the independent signer.
    pub async fn approve(
        self: Arc<Self>,
        request: Vec<u8>,
        secret: Vec<u8>,
    ) -> Result<Vec<u8>, WalletError> {
        crate::runtime::run(async move {
            let secret = crate::fixed(secret, "test authentication secret")?;
            harness(async move || {
                self.approve_request(&request, secret)
                    .await
                    .map_err(|_| WalletError::ApprovalRefused)
            })
            .await
        })
        .await
    }
    /// Wait until the enrolled service actually holds the payer's long feed read.
    pub async fn wait_for_held_feed(self: Arc<Self>) -> Result<(), WalletError> {
        crate::runtime::run(async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let held = self
                        .world
                        .lock()
                        .await
                        .as_ref()
                        .ok_or(WalletError::invalid("test service closed"))?
                        .held_reads();
                    if held > 0 {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| WalletError::invalid("service did not hold the feed read"))?
        })
        .await
    }

    /// Send a seven-unit signed checkout from the shop after the buyer admits its conversation.
    pub async fn send_checkout(self: Arc<Self>) -> Result<RequestId, WalletError> {
        crate::runtime::run(async move {
            let mut seller = self.seller.lock().await;
            let draft = invoice().map_err(paykit_sdk::PaykitError::from)?;
            Ok(Box::pin(seller.send_request(self.buyer, draft))
                .await?
                .request
                .ok_or(WalletError::invalid("signed test checkout"))?
                .into())
        })
        .await
    }
    /// Have the enrolled seller verify the original payer proof and issue its signed receipt.
    pub async fn confirm_payment(
        self: Arc<Self>,
        request: RequestId,
    ) -> Result<String, WalletError> {
        crate::runtime::run(async move {
            let request: RequestRef = request.try_into()?;
            let mut seller = self.seller.lock().await;
            for _ in 0..8 {
                if !Box::pin(seller.receive(None)).await?.report.more {
                    break;
                }
            }
            let entry = seller
                .payee_activity()
                .await?
                .into_iter()
                .find(|entry| {
                    entry.payer == self.buyer
                        && entry.request_hash == Some(request.request_hash)
                        && request.payee == self.payee
                })
                .ok_or(WalletError::invalid("original payer proof"))?;
            Box::pin(seller.confirm_payment(&entry.record))
                .await?
                .ok_or(WalletError::invalid("original seller receipt"))
        })
        .await
    }
    /// Withhold or reattach the actual enrolled payer account at its provider for signed recovery notices.
    pub async fn set_account_missing(self: Arc<Self>, missing: bool) -> Result<(), WalletError> {
        crate::runtime::run(async move {
            harness(async move || {
                let world = self.world.lock().await;
                let world = world
                    .as_ref()
                    .ok_or(WalletError::invalid("test service closed"))?;
                let buyer = world
                    .identity("buyer")
                    .map_err(|_| WalletError::invalid("test payer"))?;
                Box::pin(world.set_account_missing("https://buyer.payments.test", &buyer, missing))
                    .await
                    .map_err(|_| WalletError::invalid("test service account"))
            })
            .await
        })
        .await
    }

    /// Seed the same observed divergent-write marker used in the SDK's checkout tests.
    /// The wallet must be closed so this fixture can acquire its own protected writer.
    pub async fn mark_divergent(
        self: Arc<Self>,
        app_id: Vec<u8>,
        record: Vec<u8>,
    ) -> Result<(), WalletError> {
        crate::runtime::run(async move {
            let app_id = crate::fixed(app_id, "test app ID")?;
            let record: [u8; 32] = crate::fixed(record, "test record key")?;
            let namespace = pubky_switch_client::store::client_namespace(&app_id);
            let session =
                crate::WalletStoreSession::open(self.host.store.clone(), namespace).await?;
            let owner = session.ownership.clone();
            let key = format!("rcdiv/{}", pubky_switch_core::enc::b64(&record));
            let result = session
                .put(key.as_bytes(), br#"{"v":3,"d":null}"#.to_vec())
                .await;
            drop(session);
            owner.finish().await?;
            Ok(result?)
        })
        .await
    }

    /// Replace the buyer's lost provider through its enrolled identity signer. Leave
    /// the wallet's receive, reconciliation and user statements to the language caller.
    pub async fn replace_lost_provider(self: Arc<Self>, second: bool) -> Result<(), WalletError> {
        crate::runtime::run(async move {
            harness(async move || {
                let world = self.world.lock().await;
                let world = world
                    .as_ref()
                    .ok_or(WalletError::invalid("test service closed"))?;
                let first = "https://recovered-buyer.payments.test";
                let (old, next) = if second {
                    (first, "https://next-buyer.payments.test")
                } else {
                    ("https://buyer.payments.test", first)
                };
                let buyer = world
                    .identity("buyer")
                    .map_err(|_| WalletError::invalid("test identity"))?;
                world.advance_ms(11 * 60_000);
                world
                    .set_account_missing(old, &buyer, true)
                    .await
                    .map_err(|_| WalletError::invalid("test lost service account"))?;
                world
                    .add_switch(next, &[&buyer])
                    .await
                    .map_err(|_| WalletError::invalid("test replacement service"))?;
                let mut signer = self.signer.lock().await;
                Box::pin(signer.declare_provider_loss(
                    next,
                    pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
                ))
                .await
                .map_err(|_| WalletError::invalid("test provider loss declaration"))?;
                Box::pin(signer.end_staged_admission())
                    .await
                    .map_err(|_| WalletError::invalid("test admission"))?;
                Ok(())
            })
            .await
        })
        .await
    }

    /// Close both enrolled test services after the host shuts down its wallet.
    pub async fn finish(self: Arc<Self>) -> Result<(), WalletError> {
        crate::runtime::run(async move {
            harness(async move || {
                if let Some(world) = self.world.lock().await.take() {
                    world
                        .close()
                        .await
                        .map_err(|_| WalletError::invalid("test service shutdown"))?;
                }
                Ok(())
            })
            .await
        })
        .await
    }
}

/// What the bindings make of a method's synchronous answers, for the host tests.
#[derive(uniffi::Record)]
pub struct MethodAnswers {
    /// The candidate IDs selected; `None` when `select_endpoints` threw.
    pub selected: Option<Vec<u32>>,
    /// The transfer amount read; `None` when `transfer` threw or can't tell.
    pub transfer_amount: Option<Amount>,
    /// The settlement ID read; `None` when `settlement_id` threw or found none.
    pub settlement_id: Option<Vec<u8>>,
}

/// Call `method`'s synchronous callbacks as Paykit does for the test checkout's endpoint,
/// with `proof_json` as the proof.
#[uniffi::export]
pub fn test_method_answers(
    method: Arc<dyn PaymentMethod>,
    declaration: MethodDeclaration,
    endpoints: Vec<EndpointSupport>,
    proof_json: Vec<u8>,
) -> Result<MethodAnswers, WalletError> {
    use paykit_sdk::{CandidateId, PaymentEndpointCandidate, Rail};
    let rail = HostRail::new(
        method,
        declaration,
        crate::environment::supported_endpoints(endpoints)?,
    );
    let invalid = |_| WalletError::invalid("test checkout endpoint");
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11").map_err(invalid)?;
    let payload = PaymentEndpointPayload::new("lnbc1checkout").map_err(invalid)?;
    let amount = PaymentAmount {
        asset: Asset::new("btc").map_err(invalid)?,
        value: Decimal::new("7").map_err(invalid)?,
    };
    let candidate =
        PaymentEndpointCandidate::new(CandidateId::new(1), endpoint.clone(), payload.clone());
    let proof = serde_json::from_slice(&proof_json).map_err(|_| WalletError::invalid("proof"))?;
    Ok(MethodAnswers {
        selected: rail
            .select_endpoints(&[candidate], Some(&amount))
            .ok()
            .map(|s| s.payable.iter().map(|id| id.get()).collect()),
        transfer_amount: rail
            .transfer(&endpoint, &payload, &amount)
            .amount
            .as_ref()
            .map(Amount::from),
        settlement_id: rail.settlement_id(&proof),
    })
}
