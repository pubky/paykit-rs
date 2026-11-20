use super::{method::Account, signer, storage::DeviceStore};
use crate::*;
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{AppScopes, PayOptions, Paykit, RequestDestination, RequestDraft};
use pubky_switch_client::{
    app::{HomeserverReader, HomeserverWriter},
    store::MemoryStore,
};
use pubky_switch_core::records::AuthRequest;
use pubky_switch_harness::parties::Asking;
use pubky_switch_harness::{Backend, World};
use std::{collections::BTreeMap, sync::atomic::Ordering};
use std::{error::Error, path::Path, sync::Arc};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Home(Arc<pubky_switch_client::testing::TestHomeserver>);
#[async_trait::async_trait]
impl WalletHomeserver for Home {
    async fn get(&self, id: Vec<u8>, p: String) -> Result<Option<Vec<u8>>, HomeserverError> {
        HomeserverReader::get(self.0.as_ref(), id.try_into().unwrap(), p)
            .await
            .map_err(|_| HomeserverError::Unavailable)
    }
    async fn put(&self, id: Vec<u8>, p: String, b: Vec<u8>) -> Result<(), HomeserverError> {
        HomeserverWriter::put(self.0.as_ref(), id.try_into().unwrap(), p, b)
            .await
            .map_err(|_| HomeserverError::Unavailable)
    }
    async fn delete(&self, id: Vec<u8>, p: String) -> Result<(), HomeserverError> {
        HomeserverWriter::delete(self.0.as_ref(), id.try_into().unwrap(), p)
            .await
            .map_err(|_| HomeserverError::Unavailable)
    }
}
fn host(world: &World, root: &Path, account: Arc<Account>) -> Arc<WalletHost> {
    let host = WalletHost::new(
        Arc::new(DeviceStore::open(root)),
        Arc::new(Home(world.files().clone())),
        account.clone(),
        vec!["btc-lightning-bolt11".into()],
    )
    .unwrap();
    let mut host = Arc::into_inner(host).unwrap();
    let env = world.app_env(Arc::new(MemoryStore::new()));
    host.clock = env.clock;
    host.transport = env.transport;
    Arc::new(host)
}

async fn enrolled_payer(
    world: &World,
    root: &Path,
    account: Arc<Account>,
) -> TestResult<(Arc<WalletHost>, Arc<NativeWallet>, Vec<u8>)> {
    let id = world.identity("buyer")?.public();
    let mut buyer = Box::pin(signer::serving(world, "buyer")).await?;
    let mut other = Box::pin(signer::serving(world, "other-buyer")).await?;
    let other_id = world.identity("other-buyer")?.public();
    let reference = vec![11; 16];
    let prepare = || WalletEnrollment::new(Arc::new(DeviceStore::open(root)));
    let info = prepare().begin_enrollment(reference.clone()).await?;
    // A pending signer decision needs only the enrollment store. Even a busy
    // installed namespace must not replace that decision with a writer error.
    let physical = DeviceStore::open(root);
    let ns = pubky_switch_client::store::client_namespace(&info.app_id.clone().try_into().unwrap());
    let owner = vec![23; 32];
    assert!(physical.acquire(ns.clone(), owner.clone())?);
    assert!(matches!(
        Box::pin(host(world, root, account.clone()).complete_enrollment(reference.clone())).await,
        Err(WalletError::EnrollmentUnavailable)
    ));
    physical.release(ns, owner)?;
    let auth = AuthRequest {
        client_id: "wallet.example".into(),
        client_key: [8; 32],
        caps: format!("{}:rw", info.app_record_path),
        secret: [9; 32],
    };
    let original = prepare()
        .prepare_enrollment(
            reference.clone(),
            auth.client_id.clone(),
            auth.client_key.into(),
            auth.caps.clone(),
            auth.secret.into(),
        )
        .await?;
    let signed = serde_json::from_slice(&original.signed_request)?;
    let review = buyer.review(&signed, &auth)?;
    let approved = Box::pin(buyer.approve(review)).await?;
    let alternate = other.review(&signed, &auth)?;
    let alternate = Box::pin(other.approve(alternate)).await?;
    prepare()
        .accept_approval(reference.clone(), id.into(), approved.claim.clone())
        .await?;
    let current = host(world, root, account.clone());
    DeviceStore::hold_installation(root, true);
    assert!(matches!(
        Box::pin(current.complete_enrollment(reference.clone())).await,
        Err(WalletError::Storage { .. })
    ));
    drop(current);
    DeviceStore::hold_installation(root, false);
    let current = host(world, root, account.clone());
    assert!(matches!(
        current.load(original.app_id.clone()).await,
        Err(WalletError::NotJoined)
    ));
    assert!(matches!(
        prepare()
            .accept_approval(reference.clone(), other_id.into(), alternate.claim.clone())
            .await,
        Err(WalletError::EnrollmentConflict)
    ));
    world.advance_ms(1000);
    DeviceStore::hold_enrollment_completion(root, true);
    assert!(matches!(
        Box::pin(current.complete_enrollment(reference.clone())).await,
        Err(WalletError::Storage {
            failure: StorageError::Unavailable
        })
    ));
    drop(current);
    DeviceStore::hold_enrollment_completion(root, false);
    let current = host(world, root, account);
    assert!(matches!(
        prepare()
            .accept_approval(reference.clone(), other_id.into(), alternate.claim)
            .await,
        Err(WalletError::EnrollmentConflict)
    ));
    let installed = current.load(original.app_id.clone()).await?;
    assert_eq!(installed.identity().await?, id);
    installed.shutdown().await?;
    let wallet = Box::pin(current.complete_enrollment(reference.clone())).await?;
    buyer.claim_delivered(&approved.request).await?;
    wallet.shutdown().await?;
    prepare()
        .accept_approval(reference.clone(), id.into(), approved.claim)
        .await?;
    let wallet = Box::pin(current.complete_enrollment(reference)).await?;
    Ok((current, wallet, original.app_id))
}
async fn receive(wallet: &mut Paykit) -> TestResult {
    for _ in 0..8 {
        if !Box::pin(wallet.receive(None)).await?.report.more {
            return Ok(());
        }
    }
    Err("wallet did not finish its pages".into())
}

async fn connect_after_earlier_batch(
    world: &World,
    payer: &mut Paykit,
    shop: [u8; 32],
) -> TestResult {
    let app = payer.app_mut();
    world.lose_answer("POST", "/ops");
    let first = app.allow_connect(&shop).await.unwrap_err();
    let first_id = first.kept_batch().ok_or("missing original batch")?;
    let other = world.identity("other-shop")?.public();
    world.unreachable("POST", "/ops");
    let held = app.allow_connect(&other).await.unwrap_err();
    let pubky_switch_client::app::AppError::Ahead { ahead, .. } = &held else {
        return Err("later write did not wait for the original batch".into());
    };
    let since = ahead.kept_since_ms.ok_or("original batch was never sent")?;
    let until = ahead
        .answer_until_ms
        .ok_or("missing original answer window")?;
    let service_reply = pubky_switch_client::app::AppError::Ahead {
        ahead: *ahead,
        error: Box::new(
            pubky_switch_client::error::ClientError::Switch {
                status: 503,
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                message: "unavailable".into(),
                current: serde_json::Value::Null,
                known_version: None,
                retry_after: None,
            }
            .into(),
        ),
    };
    assert!(
        matches!(WalletError::from(service_reply), WalletError::Operation {
        kept_batch: None, service_code: Some(code), switch_refusal: Some(_), ..
    } if code == "unavailable")
    );
    let WalletError::Operation {
        kept_batch: None,
        service_code: None,
        switch_refusal: Some(rule),
        ..
    } = held.into()
    else {
        return Err("transport failure lost the earlier batch".into());
    };
    assert!(matches!(rule.kind(), SwitchRefusalKind::KeptBatchesAhead {
        batch_id, kept_since_ms: Some(start), answer_until_ms: Some(end), kept: 1,
    } if batch_id == first_id && start == since && end == until && start < end));
    Box::pin(receive(payer)).await?;
    assert!(payer.app().admits(&shop).await?);
    assert!(!payer.app().admits(&other).await?);
    Ok(())
}

async fn payment(
    world: &World,
    host: &WalletHost,
    payer: Arc<NativeWallet>,
    account: &Account,
    app_id: Vec<u8>,
) -> TestResult {
    let shop = world.identity("shop")?;
    let mut signer = Box::pin(signer::serving(world, "shop")).await?;
    let scopes = AppScopes::new().request().build()?;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&shop, "wallet")?, &names)?;
    let review = signer.review(asking.request.signed(), &asking.auth)?;
    let approved = Box::pin(signer.approve(review)).await?;
    let mut seller = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(shop.public(), &approved)?,
        host.rail.clone(),
        Arc::new(RuntimeTimer),
    ))
    .await?;
    signer.claim_delivered(&approved.request).await?;
    let buyer = world.identity("buyer")?.public();
    {
        let mut app = payer.lock().await?;
        Box::pin(receive(&mut app)).await?;
        Box::pin(connect_after_earlier_batch(world, &mut app, shop.public())).await?;
    }
    Box::pin(receive(&mut seller)).await?;
    seller.app_mut().allow_connect(&buyer).await?;
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    let payload = PaymentEndpointPayload::new("lnbc1invoice")?;
    let amount = PaymentAmount {
        asset: Asset::new("btc")?,
        value: Decimal::new("7")?,
    };
    let sent = Box::pin(seller.send_request(
        buyer,
        RequestDraft::new(
            amount,
            PaymentReference::new("checkout-7")?,
            RequestDestination::Endpoints(BTreeMap::from([(endpoint, payload)])),
        ),
    ))
    .await?;
    let request = sent.request.ok_or("missing signed request")?;
    {
        let mut original = payer.lock().await?;
        Box::pin(receive(&mut original)).await?;
        let mut paying = Box::pin(original.pay_request(&request, PayOptions::default()));
        tokio::select! { _ = account.entered.notified()=>{},_=&mut paying=>return Err("payment did not reach the interrupted method".into()) }
        assert_eq!(account.count(), 1);
    }
    drop(payer);
    let restored = host.load(app_id).await?;
    let pending = restored.payments().await?;
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].stage, PaymentStage::Paying));
    let attempt = &pending[0].attempt_id;
    account.park.store(false, Ordering::SeqCst);
    let finished = Box::pin(restored.resume_payments()).await?;
    assert!(finished
        .iter()
        .any(|o| matches!(o,PaymentOutcome::Paid{attempt_id,..} if attempt_id==attempt)));
    assert_eq!(account.count(), 1);
    assert_eq!(account.calls.load(Ordering::SeqCst), 1);
    Box::pin(receive(&mut seller)).await?;
    let activity = seller
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("missing signed payer proof")?;
    let receipt = Box::pin(seller.confirm_payment(&activity.record))
        .await?
        .ok_or("missing seller receipt")?;
    {
        let mut original = restored.lock().await?;
        Box::pin(receive(&mut original)).await?;
    }
    let receipts = restored.request_receipts(request.into()).await?;
    assert!(receipts.iter().any(|r| r.message_id == receipt
        && &r.attempt_id == attempt
        && r.payment_reference.as_deref() == Some("checkout-7")));
    assert!(restored.reconcile_allowance(vec![0; 32]).await?.is_none());
    Ok(())
}
async fn journey(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path()));
    let (env, payer, id) = Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
    assert!(matches!(
        env.load(vec![13; 16]).await,
        Err(WalletError::NotJoined)
    ));
    let other = host(world, root.path(), account.clone());
    assert!(matches!(
        other.load(id.clone()).await,
        Err(WalletError::Storage {
            failure: StorageError::InUse
        })
    ));
    let backup = payer.backup().await?;
    assert_eq!(
        paykit_sdk::PaykitBackup::from_bytes(&backup)?.to_bytes()?,
        backup
    );
    let identity = payer.identity().await?;
    let blocked_release = root.path().join("hold-release");
    std::fs::write(&blocked_release, [])?;
    // The read failed with NotJoined, but its writer could not be released.
    // The host must receive the storage failure. A later open retries that token.
    assert!(matches!(
        env.load(vec![13; 16]).await,
        Err(WalletError::Storage {
            failure: StorageError::Unavailable
        })
    ));
    assert!(matches!(
        payer.shutdown().await,
        Err(WalletError::Storage {
            failure: StorageError::Unavailable
        })
    ));
    assert!(matches!(
        other.load(id.clone()).await,
        Err(WalletError::Storage {
            failure: StorageError::InUse
        })
    ));
    std::fs::remove_file(blocked_release)?;
    assert!(matches!(
        other.load(vec![13; 16]).await,
        Err(WalletError::NotJoined)
    ));
    let reopened = env.load(id.clone()).await?;
    assert_eq!(reopened.identity().await?, identity);
    assert_eq!(reopened.app_id().await?, id);
    assert_eq!(account.count(), 0);
    Box::pin(payment(world, &env, reopened, &account, id)).await
}
fn run(backend: Backend) {
    let thread = std::thread::Builder::new()
        .stack_size(1_835_008)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                if let Some(world) = World::start(backend).unwrap() {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        Box::pin(journey(&world)),
                    )
                    .await
                    .expect("wallet journey finished");
                    world.close().await.unwrap();
                    result.unwrap();
                }
            });
        })
        .unwrap();
    thread.join().unwrap();
}
#[test]
fn test_installed_payer_reopens_the_original_approved_wallet_on_scripted() {
    run(Backend::Scripted);
}
#[test]
fn test_installed_payer_reopens_the_original_approved_wallet_on_postgresql() {
    run(Backend::Service);
}
