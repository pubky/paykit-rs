//! Host display names stay local; selling labels retain their certificate authority
//! (Dispatch spec 4.2; Paykit spec 3.1, 3.4, 8.1).
#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::app_record::PaykitMember;
use paykit_lib::bodies::RequestDestination;
use paykit_lib::{
    Asset, Decimal, Label, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Refusal, RequestDraft,
    RequestedScopes, Timer,
};
use pubky_dispatch_client::app::{AppConfig, Publication};
use pubky_dispatch_client::store::MemoryStore;
use pubky_dispatch_client::transport::BoxFuture;
use pubky_dispatch_client::{Credentials, SwitchClient};
use pubky_dispatch_core::records::{app_record_path, AppRecord};
use pubky_dispatch_core::sig::SignedDoc;
use pubky_dispatch_harness::{parties::Asking, Backend, World};
use pubky_dispatch_signer::{Protocols, Signer};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const BUYER_ORIGIN: &str = "https://buyer.labels.test";
const SELLER_ORIGIN: &str = "https://seller.labels.test";
const DAY_MS: u64 = 86_400_000;

struct TestTimer;

impl Timer for TestTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

struct Host {
    identity: &'static str,
    app: &'static str,
    display_name: &'static str,
    wallet: Paykit,
}

struct AppSpec {
    app: &'static str,
    display_name: &'static str,
    scopes: RequestedScopes,
    config: PaykitConfig,
}

async fn signer(world: &World, name: &str, origin: &str) -> TestResult<Signer> {
    let identity = world.identity(name)?;
    world.add_switch(origin, &[&identity]).await?;
    let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    let mut signer = Box::pin(Signer::create(env, world.keyring(&identity)?, origin)).await?;
    signer.resume().await?;
    Ok(signer)
}

async fn join(
    world: &World,
    signer: &mut Signer,
    identity: &'static str,
    spec: AppSpec,
    rail: &Arc<FakeRail>,
) -> TestResult<Host> {
    let owner = world.identity(identity)?;
    let keys = world.app_keys(&owner, spec.app)?;
    let scopes: Vec<_> = spec.scopes.scopes.iter().map(String::as_str).collect();
    let request = Asking::for_scopes_with_paths(
        keys,
        &scopes,
        &[],
        pubky_dispatch_core::enc::hash(spec.app.as_bytes()),
    )?;
    let review = signer.review(request.request.signed(), &request.auth)?;
    let approved = Box::pin(signer.approve(review)).await?;
    signer.claim_delivered(&approved.request).await?;
    world.advance_ms(1000);
    let wallet = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        spec.config,
        request.join_request(owner.public(), &approved)?,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    let mut host = Host {
        identity,
        app: spec.app,
        display_name: spec.display_name,
        wallet,
    };
    finish_listing(&mut host.wallet).await?;
    Ok(host)
}

async fn finish_listing(wallet: &mut Paykit) -> TestResult {
    for _ in 0..8 {
        let summary = wallet.receive(None).await?;
        if !summary.report.more && summary.records_unlisted.is_none() {
            assert!(!wallet.app().listing_owed().await?);
            return Ok(());
        }
    }
    Err("the wallet's initial listing did not finish".into())
}

fn seller(app: &'static str, display_name: &'static str, label: &str) -> TestResult<AppSpec> {
    let label = Label::parse(label)?;
    Ok(AppSpec {
        app,
        display_name,
        scopes: AppScopes::new()
            .request_label(&label)
            .manage_contacts()
            .build()?,
        config: PaykitConfig::default()
            .with_selling_label(label)
            .with_app(AppConfig {
                publishes_record: true,
                ..AppConfig::default()
            }),
    })
}

fn directory_reader(world: &World, reader: &Host, writer: &Host) -> TestResult<SwitchClient> {
    let identity = world.identity(reader.identity)?;
    Ok(SwitchClient::new(
        SELLER_ORIGIN,
        writer.wallet.identity(),
        Credentials::App {
            identity: identity.public(),
            keys: world.app_keys(&identity, reader.app)?,
            cert: reader.wallet.app().cert().clone(),
        },
        Some(writer.wallet.app().own_identity().inbox().service_key),
        world.transport(),
        world.clock().clone(),
    )?)
}

async fn published_record(
    world: &World,
    reader: &SwitchClient,
    writer: &Host,
) -> TestResult<AppRecord> {
    let app = writer.wallet.app();
    let made = AppRecord::verify(app.record(), &app.identity())?;
    let bytes = world
        .files()
        .file(app.identity(), &app_record_path(&app.app_id()))
        .ok_or("the joined app did not publish its record")?;
    let published: SignedDoc = serde_json::from_slice(&bytes)?;
    let published = AppRecord::verify(&published, &app.identity())?;
    let listed = reader
        .directory(None)
        .await?
        .apps
        .into_iter()
        .find(|entry| entry.app_id == app.app_id())
        .ok_or("the directory omitted the seller")?;
    assert_eq!(published.signed(), made.signed());
    assert_eq!(listed.record.signed(), made.signed());
    for record in [&made, &published, &listed.record] {
        let body = record.signed().body_bytes()?;
        let value: Value = serde_json::from_slice(&body)?;
        assert!(value.get("label").is_none(), "a signed display label");
        assert!(!String::from_utf8(body)?.contains(writer.display_name));
        assert_eq!(record.cert(), app.cert());
    }
    Ok(made)
}

async fn update_record(world: &World, reader: &SwitchClient, writer: &mut Host) -> TestResult {
    let before = published_record(world, reader, writer).await?;
    assert_eq!(before.expires_at(), None);
    let app_id = writer.wallet.app_id();
    world.advance_ms(1000);
    let expiry = world.now_ms() + 7 * DAY_MS;
    assert_eq!(
        writer
            .wallet
            .app_mut()
            .renew_record("paykit", Some(expiry))
            .await?,
        Publication::Published
    );
    let after = published_record(world, reader, writer).await?;
    assert_eq!(writer.wallet.app_id(), app_id);
    assert_eq!(after.expires_at(), Some(expiry));
    assert!(after.updated_at() > before.updated_at());
    assert_ne!(before.signed(), after.signed());
    assert_eq!(before.cert(), after.cert());
    assert_eq!(before.body().accepts, after.body().accepts);
    assert_eq!(before.body().protocols, after.body().protocols);
    Ok(())
}

async fn update_public_endpoints(
    world: &World,
    reader: &SwitchClient,
    writer: &mut Host,
) -> TestResult {
    assert!(writer
        .wallet
        .app()
        .cert()
        .scopes()
        .iter()
        .any(|scope| scope == "paykit_request"));
    let initial = published_record(world, reader, writer).await?;
    assert_eq!(initial.expires_at(), None);
    assert!(PaykitMember::read(&initial)?.public_endpoints.is_empty());
    let app_id = writer.wallet.app_id();
    let id = PaymentEndpointIdentifier::new("btc-lightning-bolt12")?;
    for (value, expires_at) in [
        ("lno1garden", None),
        ("lno1market", Some(world.now_ms() + 7 * DAY_MS)),
    ] {
        let before = published_record(world, reader, writer).await?;
        world.advance_ms(1000);
        let endpoints = BTreeMap::from([(id.clone(), PaymentEndpointPayload::new(value)?)]);
        assert_eq!(
            writer
                .wallet
                .set_public_endpoints(endpoints.clone(), expires_at)
                .await?,
            Publication::Published
        );
        let after = published_record(world, reader, writer).await?;
        assert_eq!(writer.wallet.app_id(), app_id);
        assert_eq!(after.expires_at(), expires_at);
        assert!(after.updated_at() > before.updated_at());
        assert_ne!(after.signed(), before.signed());
        assert_eq!(after.cert(), before.cert());
        assert_eq!(after.body().accepts, before.body().accepts);
        let previous = PaykitMember::read(&before)?;
        let current = PaykitMember::read(&after)?;
        assert_eq!(current.endpoint_identifiers, previous.endpoint_identifiers);
        assert_ne!(current.public_endpoints, previous.public_endpoints);
        assert_eq!(current.public_endpoints, endpoints);
    }
    Ok(())
}

fn draft(label: &str) -> TestResult<RequestDraft> {
    Ok(RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new(label)?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new(&format!("invoice-{label}"))?,
        )])),
    ))
}

async fn pay_seller(
    mut buyer: Host,
    seller: &mut Host,
    other: &mut Host,
    broad: &mut Host,
    label: &'static str,
) -> TestResult<Host> {
    let sent = seller
        .wallet
        .send_request(buyer.wallet.identity(), draft(label)?)
        .await?;
    let request = sent.request.ok_or("the seller did not issue its request")?;
    let buyer = tokio::spawn(Box::pin(async move {
        buyer.wallet.receive(None).await?;
        let view = buyer.wallet.request(&request).await?;
        assert_eq!(view.label.as_deref(), Some(label));
        let outcome = buyer
            .wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome?;
        assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
        Ok::<_, paykit_sdk::PaykitError>(buyer)
    }))
    .await??;
    seller.wallet.receive(None).await?;
    let sale = seller
        .wallet
        .payee_activity()
        .await?
        .into_iter()
        .find(|sale| sale.request_hash == Some(request.request_hash))
        .ok_or("the seller did not receive its payment proof")?;
    assert_eq!(sale.label.as_deref(), Some(label));
    assert_eq!(sale.status, PayeeStatus::ProofReceived);
    let excluded = other.wallet.receive(None).await?;
    assert!(excluded.applied.is_empty(), "{excluded:?}");
    assert!(other
        .wallet
        .payee_activity()
        .await?
        .iter()
        .all(|sale| sale.request_hash != Some(request.request_hash)));
    broad.wallet.receive(None).await?;
    let seen = broad
        .wallet
        .payee_activity()
        .await?
        .into_iter()
        .find(|sale| sale.request_hash == Some(request.request_hash))
        .ok_or("the broad selling app did not see the labelled sale")?;
    assert_eq!(seen.label.as_deref(), Some(label));
    assert_eq!(seen.status, PayeeStatus::ProofReceived);
    Ok(buyer)
}

async fn two_sellers(world: &World) -> TestResult {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut buyer_signer = Box::pin(signer(world, "buyer", BUYER_ORIGIN)).await?;
    let mut seller_signer = Box::pin(signer(world, "seller", SELLER_ORIGIN)).await?;
    let buyer = Box::pin(join(
        world,
        &mut buyer_signer,
        "buyer",
        AppSpec {
            app: "wallet",
            display_name: "My daily wallet",
            scopes: AppScopes::new().pay().manage_contacts().build()?,
            config: PaykitConfig::default(),
        },
        &rail,
    ))
    .await?;
    let mut first = Box::pin(join(
        world,
        &mut seller_signer,
        "seller",
        seller("first", "Garden checkout", "shop1")?,
        &rail,
    ))
    .await?;
    let mut second = Box::pin(join(
        world,
        &mut seller_signer,
        "seller",
        seller("second", "Market checkout", "shop2")?,
        &rail,
    ))
    .await?;
    let mut broad = Box::pin(join(
        world,
        &mut seller_signer,
        "seller",
        AppSpec {
            app: "overview",
            display_name: "All my sales",
            scopes: AppScopes::new().request().manage_contacts().build()?,
            config: PaykitConfig::default().with_app(AppConfig {
                publishes_record: true,
                ..AppConfig::default()
            }),
        },
        &rail,
    ))
    .await?;
    broad
        .wallet
        .app_mut()
        .invite(&buyer.wallet.identity())
        .await?;
    let mut buyer = buyer;
    buyer
        .wallet
        .app_mut()
        .invite(&first.wallet.identity())
        .await?;
    for wallet in [&mut first.wallet, &mut second.wallet, &mut broad.wallet] {
        finish_listing(wallet).await?;
    }
    for (host, own, other) in [
        (&first, "paykit_request:shop1", "paykit_request:shop2"),
        (&second, "paykit_request:shop2", "paykit_request:shop1"),
    ] {
        let scopes = host.wallet.app().cert().scopes();
        assert!(scopes.iter().any(|scope| scope == own));
        assert!(!scopes
            .iter()
            .any(|scope| scope == other || scope == "paykit_request"));
    }
    let reader = directory_reader(world, &buyer, &first)?;
    update_record(world, &reader, &mut first).await?;
    update_record(world, &reader, &mut second).await?;
    update_public_endpoints(world, &reader, &mut broad).await?;
    let buyer = Box::pin(pay_seller(
        buyer,
        &mut first,
        &mut second,
        &mut broad,
        "shop1",
    ))
    .await?;
    let buyer = Box::pin(pay_seller(
        buyer,
        &mut second,
        &mut first,
        &mut broad,
        "shop2",
    ))
    .await?;
    assert_eq!(first.wallet.payee_activity().await?.len(), 1);
    assert_eq!(second.wallet.payee_activity().await?.len(), 1);
    assert_eq!(broad.wallet.payee_activity().await?.len(), 2);

    let mut mismatched = seller("wrong-label", "Another local checkout", "shop1")?;
    mismatched.config.selling_label = Some(Label::parse("shop2")?);
    let mut wrong = Box::pin(join(world, &mut seller_signer, "seller", mismatched, &rail)).await?;
    let before = wrong.wallet.payee_activity().await?;
    assert_eq!(before, first.wallet.payee_activity().await?);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].label.as_deref(), Some("shop1"));
    let other = second.wallet.payee_activity().await?;
    assert!(before
        .iter()
        .all(|sale| sale.request_hash != other[0].request_hash));
    let queued_before = wrong.wallet.app().queued_messages("paykit").await?;
    let error = wrong
        .wallet
        .send_request(buyer.wallet.identity(), draft("shop2")?)
        .await
        .expect_err("a display name cannot authorize another seller's label");
    assert!(
        matches!(error.refusal(), Some(Refusal::Label(_))),
        "{error:?}"
    );
    assert_eq!(wrong.wallet.payee_activity().await?, before);
    assert_eq!(
        wrong.wallet.app().queued_messages("paykit").await?,
        queued_before
    );
    Ok(())
}

#[tokio::test]
async fn test_seller_authority_with_host_local_display_names() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(two_sellers(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}
