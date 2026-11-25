use super::{method::Account, signer, storage::DeviceStore};
use crate::*;
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft};
use pubky_dispatch_client::{
    app::{HomeserverReader, HomeserverWriter},
    store::MemoryStore,
};
use pubky_dispatch_core::records::AuthRequest;
use pubky_dispatch_harness::parties::Asking;
use pubky_dispatch_harness::{Backend, World};
use std::{collections::BTreeMap, sync::atomic::Ordering};
use std::{error::Error, path::Path, sync::Arc};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

mod notice;

struct Home(Arc<pubky_dispatch_client::testing::TestHomeserver>);
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
/// How long the test wallets wait for the host's `pay`, and for a payment lookup.
const PAY_LIMIT: std::time::Duration = std::time::Duration::from_millis(300);
const LOOKUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A wallet host on `root`, paying through `account`, on `world`'s service.
pub(super) fn host(world: &World, root: &Path, account: Arc<Account>) -> Arc<WalletHost> {
    configured_host(world, root, account, None)
}

fn configured_host(
    world: &World,
    root: &Path,
    account: Arc<Account>,
    config: Option<WalletConfig>,
) -> Arc<WalletHost> {
    let host = WalletHost::new(
        Arc::new(DeviceStore::open(root)),
        Arc::new(Home(world.files().clone())),
        account.clone(),
        Account::declaration(true),
        vec![EndpointSupport {
            identifier: "btc-lightning-bolt11".into(),
            fraction_digits: Some(8),
        }],
        config,
    )
    .unwrap();
    let mut host = Arc::into_inner(host).unwrap();
    let rail = account
        .rail()
        .with_pay_limit(PAY_LIMIT)
        .with_lookup_limit(LOOKUP_LIMIT);
    host.rail = Arc::new(rail);
    let env = world.app_env(Arc::new(MemoryStore::new()));
    host.clock = env.clock;
    host.transport = env.transport;
    Arc::new(host)
}

/// A payer wallet enrolled through the test signer, its host, App ID and signer.
pub(super) async fn enrolled_payer(
    world: &World,
    root: &Path,
    account: Arc<Account>,
) -> TestResult<EnrolledWallet> {
    Box::pin(enrolled_wallet(
        world,
        root,
        account,
        WalletScopes {
            pay: true,
            ..Default::default()
        },
        None,
    ))
    .await
}

type EnrolledWallet = (
    Arc<WalletHost>,
    Arc<NativeWallet>,
    Vec<u8>,
    pubky_dispatch_signer::Signer,
);

pub(super) async fn enrolled_wallet(
    world: &World,
    root: &Path,
    account: Arc<Account>,
    scopes: WalletScopes,
    config: Option<WalletConfig>,
) -> TestResult<EnrolledWallet> {
    let open = || configured_host(world, root, account.clone(), config.clone());
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
    let ns =
        pubky_dispatch_client::store::client_namespace(&info.app_id.clone().try_into().unwrap());
    let owner = vec![23; 32];
    assert!(physical.acquire(ns.clone(), owner.clone())?);
    assert!(matches!(
        Box::pin(open().complete_enrollment(reference.clone())).await,
        Err(WalletError::EnrollmentUnavailable)
    ));
    physical.release(ns, owner)?;
    let auth = AuthRequest {
        client_id: "wallet.example".into(),
        client_key: [8; 32],
        caps: format!("{}:rw", info.app_record_path),
        secret: [9; 32],
    };
    let original = prepare_scopes(&prepare(), &reference, &auth, scopes).await?;
    let signed = serde_json::from_slice(&original.signed_request)?;
    let review = buyer.review(&signed, &auth)?;
    let approved = Box::pin(buyer.approve(review)).await?;
    let alternate = other.review(&signed, &auth)?;
    let alternate = Box::pin(other.approve(alternate)).await?;
    prepare()
        .accept_approval(reference.clone(), id.into(), approved.claim.clone())
        .await?;
    let current = open();
    DeviceStore::hold_installation(root, true);
    assert!(matches!(
        Box::pin(current.complete_enrollment(reference.clone())).await,
        Err(WalletError::Storage { .. })
    ));
    drop(current);
    DeviceStore::hold_installation(root, false);
    let current = open();
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
    let current = open();
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
    Ok((current, wallet, original.app_id, buyer))
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
    // The answer was lost after the batch was sent: it may have applied.
    assert!(matches!(
        WalletError::from(first),
        WalletError::OutcomeUnknown { failure } if failure.kept_batch.as_deref() == Some(first_id.as_slice())
    ));
    let other = world.identity("other-shop")?.public();
    world.unreachable("POST", "/ops");
    let held = app.allow_connect(&other).await.unwrap_err();
    let pubky_dispatch_client::app::AppError::Ahead { ahead, .. } = &held else {
        return Err("later write did not wait for the original batch".into());
    };
    let since = ahead.kept_since_ms.ok_or("original batch was never sent")?;
    let until = ahead
        .answer_until_ms
        .ok_or("missing original answer window")?;
    let service_reply = pubky_dispatch_client::app::AppError::Ahead {
        ahead: *ahead,
        error: Box::new(
            pubky_dispatch_client::error::ClientError::Switch {
                status: 503,
                code: pubky_dispatch_core::http::ErrorCode::Unavailable,
                message: "unavailable".into(),
                current: serde_json::Value::Null,
                known_version: None,
                retry_after: None,
            }
            .into(),
        ),
    };
    // Nothing of this call was kept or sent, whatever the earlier batch's resend met.
    assert!(
        matches!(WalletError::from(service_reply), WalletError::NotSent { failure }
        if failure.kept_batch.is_none() && failure.dispatch_refusal.is_some()
            && failure.service_code.as_deref() == Some("unavailable"))
    );
    let no_answer = pubky_dispatch_client::app::AppError::Ahead {
        ahead: *ahead,
        error: Box::new(pubky_dispatch_client::app::AppError::Transport(
            pubky_dispatch_client::error::TransportError::Timeout,
        )),
    };
    assert!(
        matches!(WalletError::from(no_answer), WalletError::NotSent { failure }
        if failure.code == "transport_no_answer" && failure.kept_batch.is_none())
    );
    let WalletError::NotSent { failure } = held.into() else {
        return Err("transport failure lost the earlier batch".into());
    };
    assert!(failure.kept_batch.is_none() && failure.service_code.is_none());
    let rule = failure
        .dispatch_refusal
        .clone()
        .ok_or("transport failure lost the earlier batch")?;
    assert!(
        matches!(rule.kind(), DispatchRefusalKind::KeptBatchesAhead {
        batch_id, kept_since_ms: Some(start), answer_until_ms: Some(end), kept: 1,
    } if batch_id == first_id && start == since && end == until && start < end)
    );
    Box::pin(receive(payer)).await?;
    assert!(payer.app().admits(&shop).await?);
    assert!(!payer.app().admits(&other).await?);
    Ok(())
}

async fn received_message(wallet: &NativeWallet, expected: &str) -> TestResult<EntryReceipt> {
    let mut receipt = None;
    for _ in 0..8 {
        let report = Box::pin(wallet.receive()).await?;
        for event in report.events {
            if let WalletEventData::MessageReceived {
                msg_id,
                receipt: seen,
                ..
            } = event.data
            {
                if msg_id == expected {
                    assert!(receipt.replace(seen).is_none());
                }
            }
        }
        if !report.more {
            return receipt.ok_or_else(|| "wallet did not receive the signed message".into());
        }
    }
    Err("wallet did not finish its pages".into())
}

async fn payment(
    world: &World,
    root: &Path,
    host: &WalletHost,
    payer: Arc<NativeWallet>,
    account: &Arc<Account>,
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
    let invitation = payer
        .lend(async |app| -> Result<_, Box<dyn std::error::Error>> {
            Box::pin(receive(app)).await?;
            assert!(!app.app().admits(&shop.public()).await?);
            Ok(app
                .app_mut()
                .register_invite_code(world.now_ms() + 3_600_000, 1)
                .await?)
        })
        .await??;
    Box::pin(receive(&mut seller)).await?;
    seller.app_mut().allow_connect(&buyer).await?;
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    let payload = PaymentEndpointPayload::new("lnbc1invoice")?;
    let amount = PaymentAmount {
        asset: Asset::new("btc")?,
        value: Decimal::new("7")?,
    };
    let sent = Box::pin(
        seller.send_request(
            buyer,
            RequestDraft::new(
                amount,
                PaymentReference::new("checkout-7")?,
                RequestDestination::Endpoints(BTreeMap::from([(endpoint, payload)])),
            )
            .with_invite_code(invitation.code),
        ),
    )
    .await?;
    assert!(matches!(
        sent.delivery,
        paykit_sdk::MessageDelivery::Accepted { .. }
    ));
    let request = sent.request.ok_or("missing signed request")?;
    let delivery = Box::pin(received_message(&payer, &sent.msg_id)).await?;
    assert_eq!(
        delivery.invite_code_hash.as_deref(),
        Some(invitation.code_hash.as_slice())
    );
    assert_eq!(delivery.entry_id.len(), 16);
    assert_eq!(delivery.read_at_ms, world.now_ms());
    payer
        .lend(async |original| -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(
                delivery.service_key,
                original.app().own_identity().inbox().service_key
            );
            Box::pin(connect_after_earlier_batch(world, original, shop.public())).await?;
            Ok(())
        })
        .await??;
    // A feed wait the host stops waiting for ends its long poll at the service.
    {
        let mut waiting = Box::pin(payer.feed_wait(25));
        for _ in 0..500 {
            if world.held_reads() > 0 {
                break;
            }
            tokio::select! {
                _ = &mut waiting => return Err("the feed wait returned before holding".into()),
                () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
        assert!(world.held_reads() > 0);
    }
    for _ in 0..500 {
        if world.held_reads() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(world.held_reads(), 0);
    let views = payer.requests().await?;
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].id.request_hash, request.request_hash);
    assert_eq!(views[0].id.payee, request.payee);
    assert_eq!(views[0].payment_reference, "checkout-7");
    assert!(matches!(views[0].status, OneTimeRequestStatus::Open));
    assert!(!views[0].manual_only);
    let missing = RequestId {
        payee: request.payee.into(),
        request_hash: vec![0; 32],
    };
    let refused = Box::pin(payer.pay_request(missing, PaymentOptions::default())).await?;
    assert!(
        matches!(refused.decision, PaymentDecision::Error { error: WalletError::Permanent { failure } } if failure.refusal.as_ref().is_some_and(|r| matches!(r.kind(), RefusalKind::NotFound)))
    );
    assert!(refused.public_failures.is_empty());
    assert_eq!(account.count(), 0);
    // The host's `pay` is still running when the wallet stops waiting for it, and when the
    // host stops waiting for the wallet. Its late answer is recorded without any host call.
    {
        let mut paying =
            Box::pin(payer.pay_request(views[0].id.clone(), PaymentOptions::default()));
        tokio::select! {
            _ = account.entered.notified() => {},
            _ = &mut paying => return Err("payment did not reach the interrupted method".into()),
        }
        assert_eq!(account.count(), 1);
    }
    let pending = payer.payments().await?;
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].stage, PaymentStage::Paying));
    let attempt = &pending[0].attempt_id.clone();
    // A wallet call from inside a synchronous host callback is refused, not queued.
    let reentrant = crate::runtime::in_callback(|| super::host_calls::first_poll(payer.app_id()));
    assert!(matches!(
        reentrant,
        std::task::Poll::Ready(Err(WalletError::Reentrant))
    ));
    // The store refuses the wallet's writes when the host's `pay` answers: the first settle
    // can't record the answer and keeps it, and a later one records it once writes work.
    DeviceStore::hold_installation(root, true);
    account.park.store(false, Ordering::SeqCst);
    account.finish.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(payer
        .payments()
        .await?
        .iter()
        .any(|p| &p.attempt_id == attempt && matches!(p.stage, PaymentStage::Paying)));
    DeviceStore::hold_installation(root, false);
    let mut settled = false;
    for _ in 0..1000 {
        let now = payer.payments().await?;
        if now
            .iter()
            .any(|p| &p.attempt_id == attempt && recorded_paid(p))
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(settled, "the late answer was not recorded");
    assert_eq!(account.lookups.load(Ordering::SeqCst), 0);
    payer.shutdown().await?;
    let restored = host.load(app_id.clone()).await?;
    let finished = Box::pin(restored.resume_payments()).await?;
    assert!(!finished
        .iter()
        .any(|o| matches!(o, PaymentOutcome::Paid { attempt_id, .. } if attempt_id == attempt)));
    assert!(restored
        .payments()
        .await?
        .iter()
        .any(|p| &p.attempt_id == attempt && recorded_paid(p)));
    assert_eq!(account.count(), 1);
    assert_eq!(account.calls.load(Ordering::SeqCst), 1);
    Box::pin(receive(&mut seller)).await?;
    let activity = seller
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("missing signed payer proof")?;
    // A proof check that never answers is cut off at the lookup limit; the payee tries again.
    // The limit ends the call: the guard below only turns a missing limit into a failure, at
    // a multiple of it, so a slow machine never races the limit it checks.
    account.stall_verify.store(true, Ordering::SeqCst);
    let stalled = tokio::time::timeout(
        crate::runtime::HOST_CALL_LIMIT * 3,
        Box::pin(seller.confirm_payment(&activity.record)),
    )
    .await
    .map_err(|_| "a stalled proof check was not cut off at the lookup limit")?;
    assert!(stalled.is_err());
    account.stall_verify.store(false, Ordering::SeqCst);
    let receipt = Box::pin(seller.confirm_payment(&activity.record))
        .await?
        .ok_or("missing seller receipt")?;
    let ordinary = Box::pin(received_message(&restored, &receipt)).await?;
    assert!(ordinary.invite_code_hash.is_none());
    assert_ne!(ordinary.entry_id, delivery.entry_id);
    assert_eq!(ordinary.service_key, delivery.service_key);
    let final_views = restored.requests().await?;
    assert!(matches!(final_views[0].status, OneTimeRequestStatus::Paid));
    assert!(final_views[0]
        .receipt
        .as_ref()
        .is_some_and(|r| r.message_id == receipt));
    let receipts = restored.request_receipts(request.into()).await?;
    assert!(receipts.iter().any(|r| r.message_id == receipt
        && &r.attempt_id == attempt
        && r.payment_reference.as_deref() == Some("checkout-7")));
    assert!(restored.reconcile_allowance(vec![0; 32]).await?.is_none());
    let restored = Box::pin(later_checkout(
        &mut seller,
        root,
        host,
        restored,
        account,
        &app_id,
    ))
    .await?;
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("3")?,
        },
        PaymentReference::new("voided-checkout")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new("lnbc1voided")?,
        )])),
    );
    let other = Box::pin(seller.send_request(buyer, draft))
        .await?
        .request
        .ok_or("missing second checkout")?;
    for _ in 0..8 {
        if !Box::pin(restored.receive()).await?.more {
            break;
        }
    }
    let cancelled = restored.cancel_request(other.into(), None).await?;
    assert!(!cancelled.is_empty());
    restored.shutdown().await?;
    let reopened = host.load(app_id).await?;
    let refused = Box::pin(reopened.pay_request(other.into(), PaymentOptions::default())).await?;
    assert!(
        matches!(refused.decision, PaymentDecision::Error { error: WalletError::Permanent { failure } } if failure.refusal.as_ref().is_some_and(|r| matches!(r.kind(), RefusalKind::Cancelled)))
    );
    assert_eq!(account.count(), 3);
    assert_eq!(account.calls.load(Ordering::SeqCst), 3);
    Box::pin(alarmed_checkout(&mut seller, buyer, reopened)).await?;
    assert_eq!(account.calls.load(Ordering::SeqCst), 3);
    Ok(())
}

/// A wallet that stopped on an alarm reports the recovery for a payment, not a wait.
async fn alarmed_checkout(
    seller: &mut Paykit,
    buyer: [u8; 32],
    wallet: Arc<NativeWallet>,
) -> TestResult {
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("3")?,
        },
        PaymentReference::new("alarmed")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new("lnbc1alarmed")?,
        )])),
    );
    let request: RequestId = Box::pin(seller.send_request(buyer, draft))
        .await?
        .request
        .ok_or("missing alarmed checkout")?
        .into();
    for _ in 0..8 {
        if !Box::pin(wallet.receive()).await?.more {
            break;
        }
    }
    wallet
        .lend(async |app| {
            app.app_mut()
                .raise_rollback("paykit", "test rollback")
                .await
        })
        .await??;
    let refused = Box::pin(wallet.pay_request(request, PaymentOptions::default())).await?;
    assert!(
        matches!(&refused.decision, PaymentDecision::Error { error: WalletError::Recovery { failure } } if failure.code == "stopped"),
        "the alarm did not come as a recovery"
    );
    Ok(())
}

/// A checkout the wallet can't pay with a method without history, then pays: its `pay`
/// answers after the wallet stopped waiting and closed, and the next wallet that opens records
/// that answer without asking the host.
async fn later_checkout(
    seller: &mut Paykit,
    root: &Path,
    host: &WalletHost,
    wallet: Arc<NativeWallet>,
    account: &Arc<Account>,
    app_id: &[u8],
) -> TestResult<Arc<NativeWallet>> {
    let buyer = wallet
        .identity()
        .await?
        .try_into()
        .map_err(|_| "identity")?;
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("5")?,
        },
        PaymentReference::new("later-checkout")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new("lnbc1later")?,
        )])),
    );
    let later: RequestId = Box::pin(seller.send_request(buyer, draft))
        .await?
        .request
        .ok_or("missing later checkout")?
        .into();
    for _ in 0..8 {
        if !Box::pin(wallet.receive()).await?.more {
            break;
        }
    }
    let lookups = account.lookups.load(Ordering::SeqCst);
    let calls = account.calls.load(Ordering::SeqCst);
    // A method that can't look a payment up refuses paying, and only paying: the wallet
    // still opens.
    // A method whose guarantees change is given to a new host.
    let without_history = WalletHost {
        config: host.config.clone(),
        clock: host.clock.clone(),
        transport: host.transport.clone(),
        home: host.home.clone(),
        writer: host.writer.clone(),
        store: host.store.clone(),
        rail: Arc::new(
            account
                .rail_without_history()
                .with_pay_limit(PAY_LIMIT)
                .with_lookup_limit(LOOKUP_LIMIT),
        ),
        timer: host.timer.clone(),
    };
    wallet.shutdown().await?;
    let wallet = without_history.load(app_id.to_vec()).await?;
    let refused = Box::pin(wallet.pay_request(later.clone(), PaymentOptions::default())).await;
    assert!(matches!(
        refused,
        Err(WalletError::PaymentMethodWithoutLookup)
    ));
    let request: paykit_sdk::RequestRef = later.clone().try_into()?;
    let options: paykit_sdk::PayOptions = PaymentOptions::default().try_into()?;
    // A handoff that doesn't pass through that check is refused by the method's rail.
    let handed_off: PaymentResult = wallet
        .lend(async |app| app.pay_request(&request, options).await)
        .await?
        .into();
    assert!(matches!(
        handed_off.decision,
        PaymentDecision::Outcome {
            outcome: PaymentOutcome::Failed { .. }
        }
    ));
    assert_eq!(account.calls.load(Ordering::SeqCst), calls);
    wallet.backup().await?;
    Box::pin(wallet.receive()).await?;
    wallet.payments().await?;
    wallet.shutdown().await?;
    let wallet = host.load(app_id.to_vec()).await?;
    account.park.store(true, Ordering::SeqCst);
    {
        let mut paying = Box::pin(wallet.pay_request(later, PaymentOptions::default()));
        tokio::select! {
            _ = account.entered.notified() => {},
            _ = &mut paying => return Err("payment did not reach the parked method".into()),
        }
        // Past its limit the host's `pay` may still be paying: an open payment, not an error.
        let waited: PaymentResult = paying.await?;
        assert!(matches!(
            waited.decision,
            PaymentDecision::Outcome {
                outcome: PaymentOutcome::Pending { .. }
            }
        ));
    }
    // The wallet stops waiting at its limit and closes while the host's `pay` still runs.
    let open = wallet.payments().await?;
    let attempt = open
        .iter()
        .find(|p| matches!(p.stage, PaymentStage::Paying))
        .ok_or("the later payment is not paying")?
        .attempt_id
        .clone();
    wallet.shutdown().await?;
    account.park.store(false, Ordering::SeqCst);
    account.finish.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let reopened = host.load(app_id.to_vec()).await?;
    let mut recorded = false;
    for _ in 0..1000 {
        let now = reopened.payments().await?;
        if now
            .iter()
            .any(|p| p.attempt_id == attempt && recorded_paid(p))
        {
            recorded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        recorded,
        "the late answer was not recorded by the next wallet"
    );
    assert_eq!(account.lookups.load(Ordering::SeqCst), lookups);
    assert_eq!(account.calls.load(Ordering::SeqCst), calls + 1);
    Box::pin(lost_answer_checkout(
        seller,
        root,
        reopened.clone(),
        account,
    ))
    .await?;
    Ok(reopened)
}

/// A checkout whose `pay` charges and then throws after the wallet stopped waiting, while the
/// store refuses the wallet's writes: the resume it wakes fails, and a retry records the
/// payment from the host's lookup once writes work, with no host call to resume.
async fn lost_answer_checkout(
    seller: &mut Paykit,
    root: &Path,
    wallet: Arc<NativeWallet>,
    account: &Arc<Account>,
) -> TestResult {
    let buyer = wallet
        .identity()
        .await?
        .try_into()
        .map_err(|_| "identity")?;
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("4")?,
        },
        PaymentReference::new("lost-answer")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new("lnbc1lost")?,
        )])),
    );
    let lost: RequestId = Box::pin(seller.send_request(buyer, draft))
        .await?
        .request
        .ok_or("missing lost-answer checkout")?
        .into();
    for _ in 0..8 {
        if !Box::pin(wallet.receive()).await?.more {
            break;
        }
    }
    let lookups = account.lookups.load(Ordering::SeqCst);
    account.park.store(true, Ordering::SeqCst);
    account.throw_after_charge.store(true, Ordering::SeqCst);
    {
        let mut paying = Box::pin(wallet.pay_request(lost, PaymentOptions::default()));
        tokio::select! {
            _ = account.entered.notified() => {},
            _ = &mut paying => return Err("payment did not reach the parked method".into()),
        }
    }
    let attempt = wallet
        .payments()
        .await?
        .into_iter()
        .find(|p| matches!(p.stage, PaymentStage::Paying))
        .ok_or("the lost-answer payment is not paying")?
        .attempt_id;
    DeviceStore::hold_installation(root, true);
    account.park.store(false, Ordering::SeqCst);
    account.finish.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    DeviceStore::hold_installation(root, false);
    account.throw_after_charge.store(false, Ordering::SeqCst);
    for _ in 0..1000 {
        let now = wallet.payments().await?;
        if now
            .iter()
            .any(|p| p.attempt_id == attempt && recorded_paid(p))
        {
            assert!(account.lookups.load(Ordering::SeqCst) > lookups);
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Err("the retry did not record the payment".into())
}
/// Whether the method's "paid" answer is written for this entry.
fn recorded_paid(entry: &PaymentEntry) -> bool {
    matches!(
        entry.stage,
        PaymentStage::RailPaid | PaymentStage::AcceptanceOwed | PaymentStage::Finished
    )
}

async fn journey(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (env, payer, id, _) = Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
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
    // Bytes that aren't a backup are the caller's input, refused before any store is opened.
    assert!(matches!(
        Box::pin(env.restore(vec![14; 16], vec![1, 2, 3], BackupDevice::InPossession)).await,
        Err(WalletError::InvalidInput { field }) if field == "backup"
    ));
    let identity = payer.identity().await?;
    let blocked_release = root.path().join("hold-release");
    std::fs::write(&blocked_release, [])?;
    // This backup belongs to a different app. The failed restore must report its
    // unconfirmed release and retain the exact owner for the next open.
    let other_id = vec![12; 16];
    assert!(matches!(
        Box::pin(env.restore(other_id.clone(), backup.clone(), BackupDevice::InPossession)).await,
        Err(WalletError::Storage {
            failure: StorageError::Unavailable
        })
    ));
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
        other.load(other_id).await,
        Err(WalletError::NotJoined)
    ));
    assert!(matches!(
        other.load(vec![13; 16]).await,
        Err(WalletError::NotJoined)
    ));
    let reopened = env.load(id.clone()).await?;
    assert_eq!(reopened.identity().await?, identity);
    assert_eq!(reopened.app_id().await?, id);
    assert_eq!(account.count(), 0);
    Box::pin(payment(world, root.path(), &env, reopened, &account, id)).await
}
/// Run `journey` on a fresh `backend` world, on a thread with the wallet's stack size.
pub(super) fn run(backend: Backend, journey: impl AsyncFn(&World) -> TestResult + Send + 'static) {
    let thread = std::thread::Builder::new()
        .stack_size(1_835_008)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // No wall-clock budget for the whole journey: each wallet call is bounded by its
            // own limit, and a journey that is only slow, on a loaded machine, still passes.
            tokio::task::LocalSet::new().block_on(&runtime, async {
                if let Some(world) = World::start(backend).unwrap() {
                    let result = Box::pin(journey(&world)).await;
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
    run(Backend::Scripted, journey);
}
#[test]
fn test_installed_payer_reopens_the_original_approved_wallet_on_postgresql() {
    run(Backend::Service, journey);
}

async fn prepare_scopes(
    owner: &WalletEnrollment,
    reference: &[u8],
    auth: &AuthRequest,
    scopes: WalletScopes,
) -> Result<EnrollmentRequest, WalletError> {
    owner
        .prepare_enrollment(
            reference.to_vec(),
            auth.client_id.clone(),
            auth.client_key.into(),
            auth.caps.clone(),
            auth.secret.into(),
            scopes,
        )
        .await
}

async fn enrollment_auth(owner: &WalletEnrollment, reference: &[u8]) -> TestResult<AuthRequest> {
    let info = owner.begin_enrollment(reference.to_vec()).await?;
    Ok(AuthRequest {
        client_id: "wallet.example".into(),
        client_key: [8; 32],
        caps: format!("{}:rw", info.app_record_path),
        secret: [9; 32],
    })
}

async fn certificate_scopes(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let owner = WalletEnrollment::new(Arc::new(DeviceStore::open(root.path())));
    let mut signer = Box::pin(signer::serving(world, "buyer")).await?;
    let none = WalletScopes::default();
    let cases: [(WalletScopes, &[&str]); 9] = [
        (
            WalletScopes {
                pay: true,
                ..none.clone()
            },
            &["paykit_pay", "paykit_contacts"],
        ),
        (
            WalletScopes {
                request: true,
                ..none.clone()
            },
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        ),
        (
            WalletScopes {
                request_labels: vec!["shop".into()],
                ..none.clone()
            },
            &[
                "paykit_request:shop",
                "paykit_settlements",
                "paykit_contacts",
            ],
        ),
        (
            WalletScopes {
                allowance: true,
                ..none.clone()
            },
            &["paykit_allowance"],
        ),
        (
            WalletScopes {
                allowance_execute: true,
                ..none.clone()
            },
            &[
                "paykit_allowance_execute",
                "paykit_allowance",
                "paykit_pay",
                "paykit_contacts",
            ],
        ),
        (
            WalletScopes {
                contacts: true,
                ..none.clone()
            },
            &["paykit_contacts"],
        ),
        (
            WalletScopes {
                manage_contacts: true,
                ..none.clone()
            },
            &["paykit_manage_contacts", "paykit_contacts"],
        ),
        (
            WalletScopes {
                recovery: true,
                ..none.clone()
            },
            &["paykit_recovery"],
        ),
        (
            WalletScopes {
                extensions: vec!["paykit-cap".into()],
                ..none
            },
            &["paykit_ext:paykit-cap"],
        ),
    ];
    for (index, (scopes, expected)) in cases.into_iter().enumerate() {
        let reference = [u8::try_from(index)?; 16];
        let auth = enrollment_auth(&owner, &reference).await?;
        let request = prepare_scopes(&owner, &reference, &auth, scopes).await?;
        let signed = serde_json::from_slice(&request.signed_request)?;
        let review = signer.review(&signed, &auth)?;
        let approved = Box::pin(signer.approve(review)).await?;
        owner
            .accept_approval(reference.into(), signer.identity().into(), approved.claim)
            .await?;
        signer.claim_delivered(&approved.request).await?;
        let cert = signer
            .apps()?
            .into_iter()
            .find(|app| app.app_id == approved.app_id)
            .and_then(|app| app.cert)
            .ok_or("missing approved certificate")?;
        let actual: std::collections::BTreeSet<_> =
            cert.scopes().iter().map(String::as_str).collect();
        assert_eq!(actual, expected.iter().copied().collect());
        paykit_lib::scopes::check_certificate(&cert)?;
    }
    Ok(())
}

#[test]
fn test_wallet_scopes_match_each_approved_certificate() {
    run(Backend::Scripted, certificate_scopes);
    run(Backend::Service, certificate_scopes);
}

async fn enrollment_retry(_world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let open = || WalletEnrollment::new(Arc::new(DeviceStore::open(root.path())));
    let reference = [2; 16];
    let auth = enrollment_auth(&open(), &reference).await?;
    let none = WalletScopes::default();
    for (scopes, field_name) in [
        (none.clone(), "scopes"),
        (
            WalletScopes {
                request_labels: vec!["bad/label".into()],
                ..none.clone()
            },
            "request_labels",
        ),
        (
            WalletScopes {
                extensions: vec!["cap".into()],
                ..none.clone()
            },
            "extensions",
        ),
        (
            WalletScopes {
                request: true,
                request_labels: vec!["shop".into()],
                ..none.clone()
            },
            "scopes",
        ),
    ] {
        assert!(
            matches!(prepare_scopes(&open(), &reference, &auth, scopes).await, Err(WalletError::InvalidInput { field }) if field == field_name)
        );
    }
    let mut scopes = WalletScopes {
        pay: true,
        request_labels: vec!["one".into(), "two".into()],
        extensions: vec!["paykit-cap".into(), "paykit-shop".into()],
        ..none
    };
    let original = prepare_scopes(&open(), &reference, &auth, scopes.clone()).await?;
    scopes.contacts = true;
    scopes.extensions.reverse();
    scopes.request_labels.reverse();
    let retried = prepare_scopes(&open(), &reference, &auth, scopes.clone()).await?;
    assert_eq!(retried.signed_request, original.signed_request);
    scopes.extensions = vec!["paykit-shop".into(), "paykit-other".into()];
    assert!(matches!(
        prepare_scopes(&open(), &reference, &auth, scopes).await,
        Err(WalletError::EnrollmentConflict)
    ));
    Ok(())
}

#[test]
fn test_wallet_scopes_reject_invalid_requests_and_changed_retries() {
    run(Backend::Scripted, enrollment_retry);
    run(Backend::Service, enrollment_retry);
}

async fn wallet_config(world: &World) -> TestResult {
    use paykit_lib::{kinds::ExtensionKind, Label};
    use paykit_sdk::PaykitConfig;
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let construct = |config| {
        WalletHost::new(
            Arc::new(DeviceStore::open(root.path())),
            Arc::new(Home(world.files().clone())),
            account.clone(),
            Account::declaration(true),
            vec![],
            config,
        )
    };
    for (config, name) in [
        (
            WalletConfig {
                selling_label: Some("bad/label".into()),
                ..Default::default()
            },
            "selling_label",
        ),
        (
            WalletConfig {
                extension_kinds: vec!["paykit.payment_request".into()],
                ..Default::default()
            },
            "extension_kinds",
        ),
    ] {
        assert!(
            matches!(construct(Some(config)), Err(WalletError::InvalidInput { field }) if field == name)
        );
    }
    assert_eq!(construct(None)?.config, PaykitConfig::default());
    assert_eq!(
        construct(Some(WalletConfig::default()))?.config,
        PaykitConfig::default()
    );
    let config = WalletConfig {
        orphan_after_s: Some(3600),
        selling_label: Some("shop".into()),
        resend_budget: Some(0),
        extension_kinds: vec!["paykit-cap.offer".into()],
        publishes_record: Some(true),
    };
    let scopes = WalletScopes {
        request_labels: vec!["shop".into()],
        extensions: vec!["paykit-cap".into()],
        ..Default::default()
    };
    let (host, wallet, id, _) = Box::pin(enrolled_wallet(
        world,
        root.path(),
        account,
        scopes,
        Some(config),
    ))
    .await?;
    let mut expected = PaykitConfig::default()
        .with_orphan_after_s(3600)
        .with_selling_label(Label::new("shop")?)
        .with_resend_budget(0)
        .with_extension_kinds(vec![ExtensionKind::new("paykit-cap.offer")?]);
    expected.app.publishes_record = true;
    assert_eq!(host.config, expected);
    let check_record = async |app: &mut Paykit| -> TestResult {
        assert!(app.app().publishes_record());
        let record =
            pubky_dispatch_core::records::AppRecord::verify(app.app().record(), &app.identity())?;
        assert!(record.accepts("paykit-cap.offer"));
        Ok(())
    };
    wallet.lend(&check_record).await??;
    let backup = wallet.backup().await?;
    wallet.shutdown().await?;
    let restored = Box::pin(host.restore(id, backup, BackupDevice::KnownWiped)).await?;
    restored.lend(check_record).await??;
    restored.shutdown().await?;
    Ok(())
}

#[test]
fn test_wallet_config_applies_sdk_defaults_and_host_choices() {
    run(Backend::Scripted, wallet_config);
    run(Backend::Service, wallet_config);
}
