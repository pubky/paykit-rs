//! Another wallet of the same seller can recover an already-issued receipt after the issuing shop restarts.
#![recursion_limit = "256"]

#[path = "receipt_publication/older_backup.rs"]
mod older_backup;

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_lib::{Asset, Decimal, PaymentAmount};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, ReceiptsOf, Refusal, RequestDraft,
    RequestRef, RequestedScopes, Timer,
};
use pubky_switch_client::app::{
    Collection as SwitchCollection, RecordTarget, DEFAULT_CURSOR_DEFER_MS,
};
use pubky_switch_client::client::answers::FeedQuery;
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_client::{Credentials, SwitchClient};
use pubky_switch_core::ops::{Condition, OpKind, SignedOp};
use pubky_switch_core::shared::ApplicationRef;
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{Protocols, Signer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct TestTimer;
impl Timer for TestTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

async fn setup_identity(world: &World, name: &str, origin: &str) -> TestResult<Signer> {
    let identity = world.identity(name)?;
    world.add_switch(origin, &[&identity]).await?;
    let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    let keyring = world.keyring(&identity)?;
    let origin = origin.to_owned();
    Ok(tokio::spawn(Box::pin(async move {
        let mut signer = Signer::create(env, keyring, &origin).await?;
        signer.resume().await?;
        Ok::<_, pubky_switch_signer::SignerError>(signer)
    }))
    .await??)
}

async fn join(
    world: &World,
    mut signer: Signer,
    identity: &str,
    name: &str,
    scopes: RequestedScopes,
    store: Arc<MemoryStore>,
    rail: &Arc<FakeRail>,
) -> TestResult<(Paykit, Signer)> {
    let owner = world.identity(identity)?;
    let keys = world.app_keys(&owner, name)?;
    let auth = pubky_switch_core::enc::hash(name.as_bytes());
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let request = Asking::for_scopes_with_paths(keys, &names, &[], auth)?;
    let review = signer.review(request.request.signed(), &request.auth)?;
    let (signer, approved) = tokio::spawn(Box::pin(async move {
        let approved = signer.approve(review).await?;
        signer.claim_delivered(&approved.request).await?;
        Ok::<_, pubky_switch_signer::SignerError>((signer, approved))
    }))
    .await??;
    let join = request.join_request(owner.public(), &approved)?;
    world.advance_ms(1000);
    let mut wallet = Paykit::join(
        world.app_env(store),
        Default::default(),
        join,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    wallet.receive(None).await?;
    Ok((wallet, signer))
}

fn invoice(endpoint_id: &str, payload: &str) -> TestResult<RequestDraft> {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new(endpoint_id)?,
        PaymentEndpointPayload::new(payload)?,
    )]);
    Ok(RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new("invoice")?,
        RequestDestination::Endpoints(endpoints),
    ))
}

async fn paid_request(
    shop: &mut Paykit,
    mut buyer: Paykit,
    endpoint_id: &str,
    payload: &str,
) -> TestResult<(Paykit, RequestRef, [u8; 32])> {
    let draft = invoice(endpoint_id, payload)?;
    let request = shop
        .send_request(buyer.identity(), draft)
        .await?
        .request
        .ok_or("request was not sent")?;
    // The payer drives its own wallet task, just as each seller wallet does.
    // The shop's orchestration frame need not stay on the payer's native stack.
    let buyer = tokio::spawn(Box::pin(async move {
        buyer.receive(None).await?;
        let payment = buyer
            .pay_request(&request, PayOptions::default())
            .await
            .outcome?;
        let PayOutcome::Paid {
            proof_msg_id: Some(id),
            ..
        } = payment
        else {
            panic!("the wallet must report its actual proof, got {payment:?}");
        };
        Ok::<_, paykit_sdk::PaykitError>((buyer, id))
    }))
    .await??;
    let (buyer, proof_id) = buyer;
    let read = shop.receive(None).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("request missing")?;
    assert_eq!(
        sale.status,
        PayeeStatus::ProofReceived,
        "proof {proof_id} delivery {:?}; shop read {read:?}",
        buyer.app().delivery(&proof_id).await?
    );
    Ok((buyer, request, sale.record))
}

#[derive(Copy, Clone, Debug)]
enum Restart {
    Cold,
    Fresh,
    InPlace,
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum Shared {
    Deleted,
    ProofOnly,
    ExtendedProof,
    Issued,
}

#[tokio::test]
async fn test_reopened_shop_conditionally_publishes_over_the_exact_deletion() -> TestResult {
    on_both_services(Shared::Deleted, Restart::Cold).await
}

#[tokio::test]
async fn test_reopened_shop_conditionally_adds_its_receipt_to_the_other_wallets_record(
) -> TestResult {
    on_both_services(Shared::ProofOnly, Restart::Cold).await
}

#[tokio::test]
async fn test_reopened_shop_preserves_a_later_update_to_the_other_wallets_proof() -> TestResult {
    on_both_services(Shared::ExtendedProof, Restart::Cold).await
}

#[tokio::test]
async fn test_fresh_shop_backup_preserves_the_pending_receipt_publication() -> TestResult {
    on_both_services(Shared::ProofOnly, Restart::Fresh).await
}

#[tokio::test]
async fn test_in_place_shop_restore_preserves_the_pending_receipt_publication() -> TestResult {
    on_both_services(Shared::ProofOnly, Restart::InPlace).await
}

#[tokio::test]
async fn test_reopened_shop_accepts_an_already_published_complete_receipt() -> TestResult {
    on_both_services(Shared::Issued, Restart::Cold).await
}

async fn on_both_services(shared: Shared, restart: Restart) -> TestResult {
    with_issued_sale(async |world, sale| {
        Box::pin(publish_after_restart(world, sale, shared, restart)).await
    })
    .await
}

async fn with_issued_sale(scenario: impl AsyncFn(&World, IssuedSale) -> TestResult) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = match Box::pin(issued_sale(&world)).await {
            Ok(sale) => Box::pin(scenario(&world, sale)).await,
            Err(error) => Err(error),
        };
        world.close().await?;
        result?;
    }
    Ok(())
}

/// Inspect the signed record operation a wallet holds, with the enrolled writer's certificate.
async fn record_op(
    wallet: &Paykit,
    key: [u8; 32],
    writer: &pubky_switch_core::records::AppCert,
) -> TestResult<SignedOp> {
    wallet
        .app()
        .shared_evidence()
        .await?
        .ops
        .into_iter()
        .filter_map(|kept| SignedOp::verify_with_cert(&kept.op, writer, &wallet.identity()).ok())
        .find(|op| matches!(&op.body().kind, OpKind::RecordPut(p) if p.key == key))
        .ok_or_else(|| "missing signed record operation".into())
}

async fn service_cursor(client: &SwitchClient) -> TestResult<u64> {
    Ok(client
        .feed(FeedQuery {
            limit: Some(0),
            ..Default::default()
        })
        .await?
        .cursor)
}

async fn shared_position(client: &SwitchClient, key: &[u8; 32]) -> TestResult<(u64, u64)> {
    let record = client.record(key).await?.view;
    Ok((record.version, service_cursor(client).await?))
}

struct IssuedSale {
    seller: Signer,
    buyer: Paykit,
    shop: Paykit,
    seller_wallet: Paykit,
    store: Arc<MemoryStore>,
    rail: Arc<FakeRail>,
    request: RequestRef,
    key: [u8; 32],
    target: RecordTarget,
    observer: SwitchClient,
    original: serde_json::Value,
    proof_only: serde_json::Value,
    receipt: StoredMessage,
}

async fn issued_sale(world: &World) -> TestResult<IssuedSale> {
    const SHOP: &str = "https://shop.payment.test";
    let payer = setup_identity(world, "buyer", "https://buyer.payment.test").await?;
    let seller = setup_identity(world, "shop", SHOP).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let (mut buyer, _) = join(
        world,
        payer,
        "buyer",
        "wallet",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let store = Arc::new(MemoryStore::new());
    let (mut shop, seller) = join(
        world,
        seller,
        "shop",
        "issuer",
        AppScopes::new().request().manage_contacts().build()?,
        store.clone(),
        &rail,
    )
    .await?;
    let (mut seller_wallet, seller) = join(
        world,
        seller,
        "shop",
        "seller-wallet",
        AppScopes::new().request().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    let (buyer, request, key) = Box::pin(paid_request(
        &mut shop,
        buyer,
        "btc-lightning-bolt11",
        "ln-purchase",
    ))
    .await?;
    let identity = shop.identity();
    let observer = SwitchClient::new(
        SHOP,
        identity,
        Credentials::App {
            identity,
            keys: world.app_keys(&world.identity("shop")?, "issuer")?,
            cert: shop.app().cert().clone(),
        },
        seller
            .identity_doc()?
            .ok_or("missing identity document")?
            .service_key_of(1),
        world.transport(),
        world.clock().clone(),
    )?;
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&identity, &request.request_hash),
    );
    let prior = shop
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing paid request")?;
    let proof_only: serde_json::Value =
        serde_json::from_slice(prior.content.value().ok_or("missing proof")?)?;
    let issued = shop
        .confirm_payment(&key)
        .await?
        .ok_or("missing issued receipt")?;
    let prior = shop
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing issued activity")?;
    let mut original: serde_json::Value =
        serde_json::from_slice(prior.content.value().ok_or("missing issued record")?)?;
    original["proof"]["cert"]["merchant_note"] = "collected".into();
    shop.app_mut()
        .put_record(&target, &serde_json::to_vec(&original)?, vec![])
        .await?;
    let read = |bytes: &[u8]| ActivityRecord::read(bytes, &identity, ActivitySide::Payee);
    let receipt = read(&serde_json::to_vec(&original)?)?
        .receipt
        .ok_or("original receipt missing")?;
    assert_eq!(receipt.message().msg_id, issued);
    shop.receive(None).await?;
    seller_wallet.receive(None).await?;
    shop.app_mut().flush_cursor().await?;
    Ok(IssuedSale {
        seller,
        buyer,
        shop,
        seller_wallet,
        store,
        rail,
        request,
        key,
        target,
        observer,
        original,
        proof_only,
        receipt,
    })
}

async fn publish_after_restart(
    world: &World,
    sale: IssuedSale,
    shared: Shared,
    restart: Restart,
) -> TestResult {
    let IssuedSale {
        mut buyer,
        mut shop,
        mut seller_wallet,
        store,
        rail,
        request,
        key,
        target,
        observer,
        original,
        proof_only,
        receipt,
        ..
    } = sale;
    let identity = shop.identity();
    let id = shop.app_id();
    let cert = shop.app().cert().clone();
    let issued = receipt.message().msg_id.clone();
    let read = |bytes: &[u8]| ActivityRecord::read(bytes, &identity, ActivitySide::Payee);
    let delete = seller_wallet
        .app_mut()
        .delete_record(&target, vec![])
        .await?;
    // The shop observes the deletion before its conditional publication reaches the service.
    world.unreachable("POST", "/ops");
    shop.app_mut().receive(None).await?;
    assert_eq!(world.faults_left(), 0);
    let notice = shop
        .payee_deletion_notices()
        .await?
        .pop()
        .ok_or("missing deletion notice")?;
    assert_eq!(notice.writer, Some(seller_wallet.app_id()));
    let before = service_cursor(&observer).await?;
    assert!(
        before < delete.version.version,
        "cursor {before} passed delete {:?}",
        delete.version
    );
    shop.acknowledge_payee_deletion(&notice).await?;
    let backup = shop.backup().await?;
    let delete_ref = ApplicationRef {
        generation: delete.version.generation,
        seq: delete.version.version,
        item_hash: notice.delete,
    };
    let (current, reference) = if shared == Shared::Deleted {
        (delete.version, delete_ref)
    } else {
        let mut value = if shared == Shared::Issued {
            original.clone()
        } else {
            proof_only
        };
        value["proof"]["cert"]["shipping_note"] = "delivered".into();
        let eager = seller_wallet
            .app_mut()
            .put_record(
                &target,
                &serde_json::to_vec(&value)?,
                vec![Condition::RecordCurrent {
                    key,
                    revision: delete_ref,
                    deleted: true,
                }],
            )
            .await?;
        let op = record_op(&seller_wallet, key, seller_wallet.app().cert()).await?;
        let reference = ApplicationRef {
            generation: eager.version.generation,
            seq: eager.version.version,
            item_hash: op.item_hash()?,
        };
        if shared == Shared::ExtendedProof {
            value["proof"]["cert"]["shipment_id"] = "package-5".into();
            let updated = seller_wallet
                .app_mut()
                .put_record(
                    &target,
                    &serde_json::to_vec(&value)?,
                    vec![Condition::RecordCurrent {
                        key,
                        revision: reference,
                        deleted: false,
                    }],
                )
                .await?;
            let op = record_op(&seller_wallet, key, seller_wallet.app().cert()).await?;
            let reference = ApplicationRef {
                generation: updated.version.generation,
                seq: updated.version.version,
                item_hash: op.item_hash()?,
            };
            (updated.version, reference)
        } else {
            (eager.version, reference)
        }
    };
    drop(shop);
    let mut shop = match restart {
        Restart::Cold => {
            Box::pin(Paykit::load(
                world.app_env(store),
                Default::default(),
                id,
                rail.clone(),
                Arc::new(TestTimer),
            ))
            .await?
        }
        Restart::Fresh | Restart::InPlace => {
            let store = match restart {
                Restart::Fresh => Arc::new(MemoryStore::new()),
                _ => store,
            };
            Box::pin(Paykit::restore(
                world.app_env(store),
                Default::default(),
                &backup,
                rail.clone(),
                Arc::new(TestTimer),
            ))
            .await?
        }
    };
    let reopened = shared_position(&observer, &key).await?;
    shop.app_mut().read_record_fresh(&target).await?;
    let fresh = shared_position(&observer, &key).await?;
    shop.receive(None).await?;
    let received = shared_position(&observer, &key).await?;
    seller_wallet.app_mut().read_record_fresh(&target).await?;
    seller_wallet.receive(None).await?;
    let final_record = seller_wallet
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing final activity")?;
    assert!(!final_record.flagged && !final_record.not_taken && final_record.authenticated);
    let bytes = final_record
        .content
        .value()
        .ok_or("missing published activity")?;
    let restored = read(bytes)?.receipt.ok_or("receipt was not published")?;
    assert_eq!(restored.hash(), receipt.hash());
    assert_eq!(restored.message().msg_id, issued);
    let published: serde_json::Value = serde_json::from_slice(bytes)?;
    assert_eq!(published["proof"]["cert"]["merchant_note"], "collected");
    if shared != Shared::Deleted {
        assert_eq!(published["proof"]["cert"]["shipping_note"], "delivered");
    }
    if shared == Shared::ExtendedProof {
        assert_eq!(published["proof"]["cert"]["shipment_id"], "package-5");
    }
    if shared == Shared::Issued {
        assert_eq!(final_record.version, current);
        assert_eq!(final_record.writer, seller_wallet.app_id());
    } else {
        assert!(final_record.version.version > current.version);
        assert_eq!(final_record.writer, id);
        let signed = record_op(&seller_wallet, key, &cert).await?;
        let OpKind::RecordPut(put) = &signed.body().kind else {
            unreachable!()
        };
        assert_eq!(put.expected_version, current);
        assert_eq!(put.policy, target.collection.policy);
        assert!(signed
            .body()
            .conditions
            .contains(&Condition::RecordCurrent {
                key,
                revision: reference,
                deleted: shared == Shared::Deleted,
            }));
    }
    let at_application = [reopened, fresh, received]
        .into_iter()
        .find(|(version, _)| *version == final_record.version.version)
        .ok_or("no applied publication at the service")?;
    for (version, cursor) in [reopened, fresh, received] {
        if version < final_record.version.version {
            assert!(
                cursor < delete.version.version,
                "cursor {cursor} before publication"
            );
        }
    }
    // Publication may release the deletion before the shop has read its own replacement.
    shop.receive(None).await?;
    let next_local = shop.app().cursor().seq;
    let next_service = service_cursor(&observer).await?;
    assert!(next_local >= final_record.version.version);
    // The service can learn the committed local position on a later ordinary poll.
    world.advance_ms(DEFAULT_CURSOR_DEFER_MS + 1);
    shop.receive(None).await?;
    let after = service_cursor(&observer).await?;
    assert!(
        after >= final_record.version.version,
        "cursor {after} after reading the shared replacement"
    );
    assert!(shop.payee_deletion_notices().await?.is_empty());
    let retained = shop.retained_payee_receipts().await?;
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].receipt.hash(), receipt.hash());
    assert!(matches!(
        shop.confirm_payment(&key).await.unwrap_err().refusal(),
        Some(Refusal::CreditHistoryLost)
    ));
    buyer.receive(None).await?;
    let listed = buyer.receipts(ReceiptsOf::Request(request)).await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].msg_id, issued);
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);
    seller_wallet.app_mut().read_record_fresh(&target).await?;
    assert_eq!(
        seller_wallet
            .app()
            .shared_record(&key)
            .await?
            .ok_or("missing final record")?
            .version,
        final_record.version
    );
    println!(
        "issued receipt publication {}",
        serde_json::json!({
            "backend": format!("{:?}", world.backend()), "current": format!("{shared:?}"), "restart": format!("{restart:?}"),
            "delete": delete.version.version, "compared": current.version, "published": final_record.version.version,
            "cursor_before": before, "cursor_at_publication": at_application.1,
            "cursor_after_receive": received.1, "next_local": next_local, "next_service": next_service,
            "cursor_after": after, "receipt_hash": pubky_switch_core::enc::b64(&receipt.hash()),
            "writer": pubky_switch_core::enc::b64(&final_record.writer), "rail_executions": 1,
        })
    );
    Ok(())
}
