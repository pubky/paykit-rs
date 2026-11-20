//! A standard payer backup with the signed checkout receipt for the mobile readers.
use base64::{engine::general_purpose::STANDARD, Engine};
use paykit_lib::{Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference};
use paykit_sdk::{rail::FakeRail, AppScopes, Paykit, RequestDraft, RequestDestination, Timer};
use pubky_switch_client::{store::MemoryStore, transport::BoxFuture};
use pubky_switch_harness::{parties::Asking, Backend, World};
use std::{collections::BTreeMap,error::Error,sync::Arc};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct WalletTimer;
impl Timer for WalletTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
    }
}

async fn enrolled(world: &World, name: &str, scopes: AppScopes, rail: Arc<FakeRail>) -> Result<Paykit> {
    let identity = world.identity(name)?;
    let mut signer = super::signer::serving(world,name).await?;
    let scopes = scopes.build()?;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&identity, "wallet")?, &names)?;
    let reviewed = signer.review(asking.request.signed(), &asking.auth)?;
    let approved = Box::pin(signer.approve(reviewed)).await?;
    let app = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(identity.public(), &approved)?,
        rail,
        Arc::new(WalletTimer),
    )).await?;
    signer.claim_delivered(&approved.request).await?;
    Ok(app)
}

async fn receive(wallet: &mut Paykit) -> Result<()> {
    for _ in 0..8 {
        if !Box::pin(wallet.receive(None)).await?.report.more {
            return Ok(());
        }
    }
    Err("wallet did not complete its feed".into())
}

pub(super) async fn print_wallet() -> Result<()> {
    let world = World::start(Backend::Scripted)?.ok_or("missing wallet world")?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut payer = Box::pin(enrolled(&world,"payer",AppScopes::new().pay(),rail.clone())).await?;
    let mut seller = Box::pin(enrolled(&world,"seller",AppScopes::new().request(),rail)).await?;
    Box::pin(receive(&mut payer)).await?;
    Box::pin(receive(&mut seller)).await?;
    payer.app_mut().allow_connect(&seller.identity()).await?;
    seller.app_mut().allow_connect(&payer.identity()).await?;
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    let payload = PaymentEndpointPayload::new("lnbc1invoice")?;
    let sent = Box::pin(seller.send_request(payer.identity(), RequestDraft::new(
        PaymentAmount{asset:Asset::new("btc")?,value:Decimal::new("7")?},
        PaymentReference::new("checkout-7")?,
        RequestDestination::Endpoints(BTreeMap::from([(endpoint,payload)])),
    ))).await?;
    let request = sent.request.ok_or("missing original checkout")?;
    Box::pin(receive(&mut payer)).await?;
    let paid = Box::pin(payer.pay_request(&request,Default::default())).await.outcome?;
    let attempt = paid.attempt_id().to_text();
    Box::pin(receive(&mut seller)).await?;
    let activity = seller.payee_activity().await?.into_iter().find(|a|a.request_hash==Some(request.request_hash)).ok_or("missing payer proof")?;
    let receipt = Box::pin(seller.confirm_payment(&activity.record)).await?.ok_or("missing seller receipt")?;
    Box::pin(receive(&mut payer)).await?;
    let payments=payer.payments().await?;
    if !payments.iter().any(|entry|entry.receipt.as_ref().is_some_and(|r| r.msg_id==receipt && r.attempt_id.to_text()==attempt)) {
        return Err("payer did not hold the original signed receipt".into());
    }
    let backup = payer.backup().await?.to_bytes()?;
    println!("{}", STANDARD.encode(payer.app_id()));
    println!("{}", STANDARD.encode(payer.identity()));
    println!("{}", STANDARD.encode(request.payee));
    println!("{}", STANDARD.encode(request.request_hash));
    println!("{attempt}");
    println!("{receipt}");
    println!("{}", STANDARD.encode(backup));
    world.close().await?;
    Ok(())
}
