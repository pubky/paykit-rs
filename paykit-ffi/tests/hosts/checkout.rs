//! An enrolled shop for exercising the native host clients on the local Switch harness.
use crate::{
    HostRail, PaymentMethod, RequestId, RuntimeTimer, WalletError, WalletHost, WalletStore,
};
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft, RequestRef};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_core::{
    records::{AppRequest, AuthRequest},
    sig::SignedDoc,
};
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::Signer;
use std::{collections::BTreeMap, error::Error, sync::Arc};
use tokio::sync::Mutex;

use crate::fixture_signer as signer;

/// A local seller and independent approving signer, available with the test-wallet feature.
#[derive(uniffi::Object)]
pub struct CheckoutFixture {
    world: World,
    host: Arc<WalletHost>,
    signer: Mutex<Signer>,
    seller: Mutex<Paykit>,
    buyer: [u8; 32],
    payee: [u8; 32],
}

impl CheckoutFixture {
    async fn open(
        store: Arc<dyn WalletStore>,
        method: Arc<dyn PaymentMethod>,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        let world = World::start(Backend::Scripted)?.ok_or("no scripted Switch")?;
        let buyer = world.identity("buyer")?.public();
        let owner = world.identity("shop")?;
        let signer = Box::pin(signer::serving(&world, "buyer")).await?;
        let mut issuer = Box::pin(signer::serving(&world, "shop")).await?;
        let rail = Arc::new(HostRail {
            method,
            identifiers: vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11")?],
        });
        let host = Arc::new(WalletHost {
            clock: world.clock().clone(),
            transport: world.transport(),
            home: world.files().clone(),
            writer: world.files().clone(),
            store,
            rail: rail.clone(),
            timer: Arc::new(RuntimeTimer),
        });
        let scopes = AppScopes::new().request().build()?;
        let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
        let asking = Asking::for_scopes(world.app_keys(&owner, "till")?, &names)?;
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
            world,
            host,
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
        signer.claim_delivered(&approved.request).await?;
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

/// Enroll a local shop and use the supplied foreign store and method on the buyer's wallet.
#[uniffi::export(async_runtime = "tokio")]
pub async fn enrolled_test_checkout(
    store: Arc<dyn WalletStore>,
    method: Arc<dyn PaymentMethod>,
) -> Result<Arc<CheckoutFixture>, WalletError> {
    harness(async move || {
        CheckoutFixture::open(store, method)
            .await
            .map_err(|_| WalletError::invalid("test checkout setup"))
    })
    .await
}

#[uniffi::export(async_runtime = "tokio")]
impl CheckoutFixture {
    /// Use the original SDK wallet and host callbacks with the local test services.
    pub fn host(&self) -> Arc<WalletHost> {
        self.host.clone()
    }
    /// The identity that signs the buyer's enrollment.
    pub fn identity(&self) -> Vec<u8> {
        self.buyer.into()
    }
    /// The identity of the enrolled shop.
    pub fn payee(&self) -> Vec<u8> {
        self.payee.into()
    }
    /// Approve the actual host request using the independent signer.
    pub async fn approve(
        self: Arc<Self>,
        request: Vec<u8>,
        secret: Vec<u8>,
    ) -> Result<Vec<u8>, WalletError> {
        let secret = crate::fixed(secret, "test authentication secret")?;
        harness(async move || {
            self.approve_request(&request, secret)
                .await
                .map_err(|_| WalletError::ApprovalRefused)
        })
        .await
    }
    /// Send a seven-unit signed checkout from the shop after the buyer admits its conversation.
    pub async fn send_checkout(&self) -> Result<RequestId, WalletError> {
        let mut seller = self.seller.lock().await;
        let draft = invoice().map_err(paykit_sdk::PaykitError::from)?;
        Ok(Box::pin(seller.send_request(self.buyer, draft))
            .await?
            .request
            .ok_or(WalletError::invalid("signed test checkout"))?
            .into())
    }
    /// Have the enrolled seller verify the original payer proof and issue its signed receipt.
    pub async fn confirm_payment(&self, request: RequestId) -> Result<String, WalletError> {
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
    }
    /// Withhold or reattach the actual enrolled payer account at its provider for signed recovery notices.
    pub async fn set_account_missing(self: Arc<Self>, missing: bool) -> Result<(), WalletError> {
        harness(async move || {
            let buyer = self
                .world
                .identity("buyer")
                .map_err(|_| WalletError::invalid("test payer"))?;
            Box::pin(
                self.world
                    .set_account_missing("https://buyer.payments.test", &buyer, missing),
            )
            .await
            .map_err(|_| WalletError::invalid("test service account"))
        })
        .await
    }
}
