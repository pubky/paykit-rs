//! An installed payer with the original signed checkout receipt for the mobile readers.
use base64::{engine::general_purpose::STANDARD, Engine};
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{rail::FakeRail, AppScopes, Paykit, RequestDestination, RequestDraft, Timer};
use pubky_dispatch_client::{
    store::{client_namespace, MemoryStore, StateStore},
    transport::BoxFuture,
};
use pubky_dispatch_harness::{parties::Asking, World};
use std::{
    collections::BTreeMap,
    error::Error,
    io::{self, Write},
    sync::Arc,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct WalletTimer;
impl Timer for WalletTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
    }
}

async fn enrolled(
    world: &World,
    name: &str,
    scopes: AppScopes,
    rail: Arc<FakeRail>,
    store: Arc<MemoryStore>,
) -> Result<(Paykit, pubky_dispatch_signer::Signer)> {
    let identity = world.identity(name)?;
    let mut signer = super::signer::serving(world, name).await?;
    let scopes = scopes.build()?;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&identity, "wallet")?, &names)?;
    let reviewed = signer.review(asking.request.signed(), &asking.auth)?;
    let approved = Box::pin(signer.approve(reviewed)).await?;
    let app = Box::pin(Paykit::join(
        world.app_env(store),
        Default::default(),
        asking.join_request(identity.public(), &approved)?,
        rail,
        Arc::new(WalletTimer),
    ))
    .await?;
    signer.claim_delivered(&approved.request).await?;
    Ok((app, signer))
}

async fn receive(wallet: &mut Paykit) -> Result<()> {
    for _ in 0..8 {
        if !Box::pin(wallet.receive(None)).await?.report.more {
            return Ok(());
        }
    }
    Err("wallet did not complete its feed".into())
}

/// An installed wallet whose next resume must report the original Paykit or Dispatch refusal.
pub(super) async fn print_resume_refusal(left: bool) -> Result<()> {
    let world = super::service::start()?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let source = Arc::new(MemoryStore::new());
    let (mut payer, _) = Box::pin(enrolled(
        &world,
        "payer",
        AppScopes::new().pay(),
        rail.clone(),
        source.clone(),
    ))
    .await?;
    Box::pin(receive(&mut payer)).await?;
    let app = payer.app_id();
    let installed = if left {
        Box::pin(
            payer
                .app_mut()
                .leave(pubky_dispatch_client::app::LeaveMode::TwoStep),
        )
        .await?;
        drop(payer);
        source
    } else {
        let backup = payer.backup().await?;
        drop(payer);
        let installed = Arc::new(MemoryStore::new());
        let restored = Box::pin(Paykit::restore(
            world.app_env(installed.clone()),
            Default::default(),
            &backup,
            rail,
            Arc::new(WalletTimer),
        ))
        .await?;
        drop(restored);
        installed
    };
    {
        let mut output = io::stdout().lock();
        writeln!(output, "{}", STANDARD.encode(app))?;
        write_store(&mut output, app, &installed)?;
    }
    world.close().await?;
    Ok(())
}

pub(super) async fn print_wallet(seller_only: bool) -> Result<()> {
    let world = super::service::start()?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let payer_store = Arc::new(MemoryStore::new());
    let (mut payer, _) = Box::pin(enrolled(
        &world,
        "payer",
        AppScopes::new().pay(),
        rail.clone(),
        payer_store.clone(),
    ))
    .await?;
    let seller_store = Arc::new(MemoryStore::new());
    let (mut seller, _) = Box::pin(enrolled(
        &world,
        "seller",
        AppScopes::new().request(),
        rail,
        seller_store.clone(),
    ))
    .await?;
    Box::pin(receive(&mut payer)).await?;
    Box::pin(receive(&mut seller)).await?;
    payer.app_mut().allow_connect(&seller.identity()).await?;
    seller.app_mut().allow_connect(&payer.identity()).await?;
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    let payload = PaymentEndpointPayload::new("lnbc1invoice")?;
    let sent = Box::pin(seller.send_request(
        payer.identity(),
        RequestDraft::new(
            PaymentAmount {
                asset: Asset::new("btc")?,
                value: Decimal::new("7")?,
            },
            PaymentReference::new("checkout-7")?,
            RequestDestination::Endpoints(BTreeMap::from([(endpoint, payload)])),
        ),
    ))
    .await?;
    let request = sent.request.ok_or("missing original checkout")?;
    Box::pin(receive(&mut payer)).await?;
    let paid = Box::pin(payer.pay_request(&request, Default::default()))
        .await
        .outcome?;
    let attempt = paid.attempt_id().to_text();
    Box::pin(receive(&mut seller)).await?;
    let activity = seller
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("missing payer proof")?;
    let receipt = Box::pin(seller.confirm_payment(&activity.record))
        .await?
        .ok_or("missing seller receipt")?;
    Box::pin(receive(&mut payer)).await?;
    let payments = payer.payments().await?;
    if !payments.iter().any(|entry| {
        entry
            .receipt
            .as_ref()
            .is_some_and(|r| r.msg_id == receipt && r.attempt_id.to_text() == attempt)
    }) {
        return Err("payer did not hold the original signed receipt".into());
    }
    if seller_only {
        let app = seller.app_id();
        drop(seller);
        let mut output = io::stdout().lock();
        writeln!(output, "{}", STANDARD.encode(app))?;
        write_store(&mut output, app, &seller_store)?;
    } else {
        let app = payer.app_id();
        let mut output = io::stdout().lock();
        writeln!(output, "{}", STANDARD.encode(app))?;
        writeln!(output, "{}", STANDARD.encode(payer.identity()))?;
        writeln!(output, "{}", STANDARD.encode(request.payee))?;
        writeln!(output, "{}", STANDARD.encode(request.request_hash))?;
        writeln!(output, "{attempt}")?;
        writeln!(output, "{receipt}")?;
        drop(payer);
        write_store(&mut output, app, &payer_store)?;
    }
    world.close().await?;
    Ok(())
}

fn write_store(output: &mut impl Write, app: [u8; 16], store: &MemoryStore) -> Result<()> {
    let namespace = client_namespace(&app);
    writeln!(output, "{namespace}")?;
    for (key, value) in store.scan(&namespace, b"", None, u32::MAX)? {
        writeln!(
            output,
            "{} {}",
            STANDARD.encode(key),
            STANDARD.encode(value)
        )?;
    }
    output.flush()?;
    Ok(())
}

/// Hand over the protected state with one genuine Paykit duty after the native wallet closes.
pub(super) async fn print_notice(cleared: bool) -> Result<()> {
    use pubky_dispatch_client::app::AppEvent;

    let world = super::service::start()?;
    let store = Arc::new(MemoryStore::new());
    let owner = world.identity("payer")?;
    let (mut payer, mut signer) = Box::pin(enrolled(
        &world,
        "payer",
        AppScopes::new().pay(),
        Arc::new(FakeRail::with_clock(world.clock().clone())),
        store.clone(),
    ))
    .await?;
    Box::pin(receive(&mut payer)).await?;
    Box::pin(world.set_account_missing("https://payer.payments.test", &owner, true)).await?;
    let stopped = Box::pin(payer.receive(None)).await?.report;
    let notice = payer
        .app()
        .recovery_required()
        .ok_or("no verified notice")?;
    let mut duty = stopped
        .events
        .iter()
        .find(|event| {
            matches!(event,
                AppEvent::RecoveryRequired { notice: seen, .. } if *seen == notice
            )
        })
        .and_then(AppEvent::id)
        .ok_or("missing original notice duty")?;
    if cleared {
        payer.app_mut().ack_events_for("paykit", duty).await?;
        let successor = "https://new-payer.payments.test";
        world.add_switch(successor, &[&owner]).await?;
        let current = Box::pin(signer.declare_provider_loss(
            successor,
            pubky_dispatch_signer::UserConfirmed::shown_and_confirmed(),
        ))
        .await?;
        let mut clearance = None;
        for _ in 0..8 {
            let report = Box::pin(payer.receive(None)).await?.report;
            clearance = report.events.iter().find(|event| matches!(event,
                AppEvent::RecoveryNoticeCleared { generation, .. } if *generation == notice.generation
            )).and_then(AppEvent::id);
            if clearance.is_some() {
                assert_eq!(report.cursor.generation, current.generation());
                assert!(report.cursor.generation > notice.generation);
                break;
            }
        }
        duty = clearance.ok_or("missing clearance for the original generation")?;
    }
    let app_id = payer.app_id();
    drop(payer);
    {
        let mut output = io::stdout().lock();
        writeln!(output, "{}", STANDARD.encode(app_id))?;
        writeln!(output, "{}", notice.generation)?;
        writeln!(output, "{}", notice.first_seen_ms)?;
        writeln!(output, "{}", notice.service_time_ms)?;
        writeln!(output, "{}", STANDARD.encode(duty.to_bytes()))?;
        write_store(&mut output, app_id, &store)?;
    }
    world.close().await?;
    Ok(())
}
