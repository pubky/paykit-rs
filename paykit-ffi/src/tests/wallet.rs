use super::{method::Account, signer, storage::DeviceStore};
use crate::*;
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft};
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

mod notice;

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
/// How long the test wallets wait for the host's `pay`, and for a payment lookup.
const PAY_LIMIT: std::time::Duration = std::time::Duration::from_millis(300);
const LOOKUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

fn host(world: &World, root: &Path, account: Arc<Account>) -> Arc<WalletHost> {
    let host = WalletHost::new(
        Arc::new(DeviceStore::open(root)),
        Arc::new(Home(world.files().clone())),
        account.clone(),
        Account::declaration(true),
        vec![EndpointSupport {
            identifier: "btc-lightning-bolt11".into(),
            fraction_digits: Some(8),
        }],
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

async fn enrolled_payer(
    world: &World,
    root: &Path,
    account: Arc<Account>,
) -> TestResult<(
    Arc<WalletHost>,
    Arc<NativeWallet>,
    Vec<u8>,
    pubky_switch_signer::Signer,
)> {
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
        matches!(WalletError::from(service_reply), WalletError::RetryLater { failure }
        if failure.kept_batch.is_none() && failure.switch_refusal.is_some()
            && failure.service_code.as_deref() == Some("unavailable"))
    );
    let WalletError::RetryLater { failure } = held.into() else {
        return Err("transport failure lost the earlier batch".into());
    };
    assert!(failure.kept_batch.is_none() && failure.service_code.is_none());
    let rule = failure
        .switch_refusal
        .clone()
        .ok_or("transport failure lost the earlier batch")?;
    assert!(matches!(rule.kind(), SwitchRefusalKind::KeptBatchesAhead {
        batch_id, kept_since_ms: Some(start), answer_until_ms: Some(end), kept: 1,
    } if batch_id == first_id && start == since && end == until && start < end));
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
    // While the host's `pay` runs, a resume neither pays the attempt again nor asks the host.
    Box::pin(payer.resume_payments()).await?;
    assert_eq!(account.calls.load(Ordering::SeqCst), 1);
    assert_eq!(account.lookups.load(Ordering::SeqCst), 0);
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
    account.stall_verify.store(true, Ordering::SeqCst);
    let stalled = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Box::pin(seller.confirm_payment(&activity.record)),
    )
    .await
    .map_err(|_| "a stalled proof check was not cut off")?;
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
    // A dispatch that doesn't pass through that check is refused by the method's rail.
    let dispatched: PaymentResult = wallet
        .lend(async |app| app.pay_request(&request, options).await)
        .await?
        .into();
    assert!(matches!(
        dispatched.decision,
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
fn run(backend: Backend, journey: impl AsyncFn(&World) -> TestResult + Send + 'static) {
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
    run(Backend::Scripted, journey);
}
#[test]
fn test_installed_payer_reopens_the_original_approved_wallet_on_postgresql() {
    run(Backend::Service, journey);
}
