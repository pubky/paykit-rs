//! Independent tills confirm a shared invoice settlement through the ordinary wallet.
#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, occurrence_tag, settlement_bucket, settlement_tag};
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::records::{OccurrenceKind, SettlementBucket, SettlementMark};
use paykit_lib::{Asset, Decimal, Label, PaymentAmount};
use paykit_sdk::rail::{Behaviour, FakeRail, Rail};
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, ReceiptDetails, ReceiptsOf,
    RequestDraft, RequestRef, RequestedScopes, Timer,
};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::transport::BoxFuture;
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

async fn paid_request(
    shop: &mut Paykit,
    mut buyer: Paykit,
    endpoint_id: &str,
    payload: &str,
) -> TestResult<(Paykit, RequestRef, [u8; 32])> {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new(endpoint_id)?,
        PaymentEndpointPayload::new(payload)?,
    )]);
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new("invoice")?,
        RequestDestination::Endpoints(endpoints),
    );
    let request = shop
        .send_request(buyer.identity(), draft)
        .await?
        .request
        .ok_or("request was not sent")?;
    // The payer drives its own wallet task, just as each independent till does.
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

#[tokio::test]
async fn test_two_tills_confirm_one_paid_request() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(competing_tills(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

struct Tills {
    buyer: Paykit,
    other: Paykit,
    till: Paykit,
    labelled: Paykit,
    rail: Arc<FakeRail>,
    till_store: Arc<MemoryStore>,
    labelled_store: Arc<MemoryStore>,
    signer: Box<Signer>,
}

async fn enrolled_tills(world: &World) -> TestResult<Tills> {
    let first = setup_identity(world, "buyer", "https://buyer.payment.test").await?;
    let second = setup_identity(world, "other-buyer", "https://other-buyer.payment.test").await?;
    let shop = setup_identity(world, "shop", "https://shop.payment.test").await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let (mut buyer, _) = join(
        world,
        first,
        "buyer",
        "first-buyer",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let (mut other, _) = join(
        world,
        second,
        "other-buyer",
        "second-buyer",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let till_store = Arc::new(MemoryStore::new());
    let (mut till, shop) = join(
        world,
        shop,
        "shop",
        "plain-till",
        AppScopes::new().request().manage_contacts().build()?,
        till_store.clone(),
        &rail,
    )
    .await?;
    let labelled_store = Arc::new(MemoryStore::new());
    let (labelled, signer) = join(
        world,
        shop,
        "shop",
        "labelled-till",
        AppScopes::new()
            .request_label(&Label::parse("shop1")?)
            .manage_contacts()
            .build()?,
        labelled_store.clone(),
        &rail,
    )
    .await?;
    for wallet in [&mut buyer, &mut other] {
        wallet.app_mut().invite(&till.identity()).await?;
        till.app_mut().invite(&wallet.identity()).await?;
    }
    Ok(Tills {
        buyer,
        other,
        till,
        labelled,
        rail,
        till_store,
        labelled_store,
        signer: Box::new(signer),
    })
}

async fn competing_tills(world: &World) -> TestResult {
    let Tills {
        buyer,
        other,
        mut till,
        mut labelled,
        rail,
        till_store,
        labelled_store,
        ..
    } = Box::pin(enrolled_tills(world)).await?;
    let before_credit = till.backup().await?;
    // Both authorized tills see the same legitimate purchase in the shop's labelled activity.
    let (mut buyer, winner, first_record) = Box::pin(paid_request(
        &mut labelled,
        buyer,
        "btc-lightning-bolt11",
        "ln-one-invoice",
    ))
    .await?;
    till.receive(None).await?;
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);
    assert!(till.verify_payment(&first_record).await?.is_some());
    assert!(labelled.verify_payment(&first_record).await?.is_some());
    let (first_receipt, second_receipt) = tokio::join!(
        Box::pin(till.confirm_payment(&first_record)),
        Box::pin(labelled.confirm_payment(&first_record))
    );
    let receipt = first_receipt?.ok_or("first till could not confirm the paid request")?;
    assert_eq!(second_receipt?, Some(receipt.clone()));
    buyer.receive(None).await?;
    let receipts = buyer.receipts(ReceiptsOf::Request(winner)).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, receipt);
    let first_id = till.app_id();
    let second_id = labelled.app_id();
    drop((till, labelled));
    let mut reopened = Paykit::load(
        world.app_env(till_store.clone()),
        Default::default(),
        first_id,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    let mut other_till = Paykit::load(
        world.app_env(labelled_store),
        Default::default(),
        second_id,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    reopened.receive(None).await?;
    other_till.receive(None).await?;
    assert_eq!(
        Box::pin(reopened.confirm_payment(&first_record)).await?,
        Some(receipt.clone())
    );
    assert_eq!(
        Box::pin(other_till.confirm_payment(&first_record)).await?,
        Some(receipt.clone())
    );
    let checks = rail.proof_checks();
    let id = rail
        .settlement_id(&checks[0].proof)
        .ok_or("verified invoice has no settlement")?;
    for reader in [&reopened, &other_till] {
        let naming = reader.app().naming_key();
        let tag = settlement_tag(naming, &id);
        let number = settlement_bucket(&tag);
        let key = SettlementBucket::record_key(number, naming)?;
        let records = reader
            .app()
            .records_of(Collection::Settlements.name())
            .await?;
        let bucket = records
            .iter()
            .find(|r| r.key == key)
            .ok_or("settlement record missing")?;
        assert_eq!(bucket.policy, Collection::Settlements.fixed_policy());
        let view = SettlementBucket::bind(
            bucket.content.value().ok_or("bucket unreadable")?,
            number,
            naming,
            &key,
        )?;
        let occurrence = occurrence_tag(
            naming,
            &natural_key::request_record(&reader.identity(), &winner.request_hash),
        );
        let mark = SettlementMark::new(&tag, OccurrenceKind::OneTimeRequest, occurrence);
        assert_eq!(view.marks(), &[mark]);
        let retained = format!(
            "sm/{}/{}",
            pubky_switch_core::enc::b64(&key),
            pubky_switch_core::enc::b64(&mark.to_bytes())
        );
        assert!(reader
            .app()
            .protocol_get("paykit", retained.as_bytes())
            .await?
            .is_some());
    }
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);

    let (mut other, second, second_record) = Box::pin(paid_request(
        &mut reopened,
        other,
        "btc-lightning-bolt11",
        "ln-independent-paid",
    ))
    .await?;
    let second_receipt = reopened
        .confirm_payment(&second_record)
        .await?
        .ok_or("second receipt missing")?;
    other.receive(None).await?;
    let receipts = other.receipts(ReceiptsOf::Request(second)).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, second_receipt);
    assert_ne!(receipts[0].msg_id, receipt);
    assert_eq!(rail.all_executions().values().sum::<u64>(), 2);

    let (mut other, independent, third_record) = Box::pin(paid_request(
        &mut other_till,
        other,
        "btc-lightning-bolt11",
        "ln-independent-invoice",
    ))
    .await?;
    let stored = other_till
        .confirm_payment_with(&third_record, ReceiptDetails::new().store_only())
        .await?
        .ok_or("independent credit missing")?;
    assert!(other_till.app().delivery(&stored).await?.is_none());
    other.receive(None).await?;
    assert!(other
        .receipts(ReceiptsOf::Request(independent))
        .await?
        .is_empty());
    let backup = other_till.backup().await?;
    drop(other_till);
    let mut restored = Paykit::restore(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        &backup,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    restored.receive(None).await?;
    assert_eq!(
        restored.confirm_payment(&third_record).await?,
        Some(stored.clone())
    );
    assert!(restored.app().delivery(&stored).await?.is_none());
    assert!(restored
        .notices()
        .await?
        .iter()
        .all(|n| { n.msg_id.as_deref() != Some(stored.as_str()) }));
    other.receive(None).await?;
    assert!(other
        .receipts(ReceiptsOf::Request(independent))
        .await?
        .is_empty());
    assert_eq!(rail.all_executions().values().sum::<u64>(), 3);
    // Selecting a backup predating these credits keeps the marks this installation
    // already saw. A new store learns them from its own authenticated read.
    reopened.receive(None).await?;
    let marks = reopened.app().protocol_scan("paykit", b"sm/").await?;
    assert_eq!(marks.len(), 3);
    drop(reopened);
    let in_place = Paykit::restore(
        world.app_env(till_store),
        Default::default(),
        &before_credit,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert_eq!(in_place.app().protocol_scan("paykit", b"sm/").await?, marks);
    drop(in_place);
    let mut fresh = Paykit::restore(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        &before_credit,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert!(fresh
        .app()
        .protocol_scan("paykit", b"sm/")
        .await?
        .is_empty());
    fresh.receive(None).await?;
    assert_eq!(fresh.app().protocol_scan("paykit", b"sm/").await?, marks);
    Ok(())
}

#[tokio::test]
async fn test_different_requests_compete_for_one_transaction_in_a_new_and_current_bucket(
) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(two_transactions(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

/// Distinct method transactions whose marks share a physical bucket.
fn transactions_in_one_bucket<const N: usize>(
    naming: &pubky_switch_core::ident::NamingKey,
) -> TestResult<[([u8; 32], [u8; 32]); N]> {
    let first = settlement_tag(naming, format!("{:064x}", 1).as_bytes());
    let mut same = Vec::new();
    for number in 1..100_000u64 {
        let tag = settlement_tag(naming, format!("{number:064x}").as_bytes());
        if settlement_bucket(&tag) == settlement_bucket(&first) {
            let mut id = [0; 32];
            id[24..].copy_from_slice(&number.to_be_bytes());
            same.push((id, tag));
            if same.len() == N {
                break;
            }
        }
    }
    same.try_into()
        .map_err(|_| "too few test transactions in the bucket".into())
}

/// Two independent transactions in the same bucket. Each has two payments and one
/// method settlement ID under Paykit wire 1.5.
async fn two_transactions(world: &World) -> TestResult {
    let mut wallets = Box::pin(enrolled_tills(world)).await?;
    let naming = wallets.till.app().naming_key();
    let [first, second] = transactions_in_one_bucket(naming)?;
    let number = settlement_bucket(&first.1);
    let key = SettlementBucket::record_key(number, naming)?;
    assert!(wallets.till.app().shared_record(&key).await?.is_none());
    let mut credited = Vec::new();
    for (txid, tag) in [first, second] {
        wallets.rail.next(Behaviour::InTransaction(txid));
        let (buyer, plain, plain_key) = Box::pin(paid_request(
            &mut wallets.till,
            wallets.buyer,
            "btc-bitcoin-p2wpkh",
            "bcrt1qqyqszqgpqyqszqgpqyqszqgpqyqszqgpvxat9t",
        ))
        .await?;
        wallets.rail.next(Behaviour::InTransaction(txid));
        wallets.buyer = buyer;
        let (other, label, label_key) = Box::pin(paid_request(
            &mut wallets.labelled,
            wallets.other,
            "btc-bitcoin-p2wpkh",
            "bcrt1qqgpqyqszqgpqyqszqgpqyqszqgpqyqszazmwwa",
        ))
        .await?;
        wallets.other = other;
        assert_ne!(plain_key, label_key);
        let (a, b) = tokio::join!(
            Box::pin(wallets.till.confirm_payment(&plain_key)),
            Box::pin(wallets.labelled.confirm_payment(&label_key)),
        );
        let (a, b) = (a?, b?);
        assert_ne!(a.is_some(), b.is_some());
        wallets.buyer.receive(None).await?;
        wallets.other.receive(None).await?;
        let receipt_ids = |receipts: Vec<paykit_sdk::ReceivedReceipt>| {
            receipts.into_iter().map(|r| r.msg_id).collect::<Vec<_>>()
        };
        let observed = wallets.buyer.receipts(ReceiptsOf::Request(plain)).await?;
        assert_eq!(receipt_ids(observed), a.iter().cloned().collect::<Vec<_>>());
        let observed = wallets.other.receipts(ReceiptsOf::Request(label)).await?;
        assert_eq!(receipt_ids(observed), b.iter().cloned().collect::<Vec<_>>());
        let winner = if a.is_some() { plain } else { label };
        let natural = natural_key::request_record(&wallets.till.identity(), &winner.request_hash);
        let winner_tag = occurrence_tag(wallets.till.app().naming_key(), &natural);
        credited.push(SettlementMark::new(
            &tag,
            OccurrenceKind::OneTimeRequest,
            winner_tag,
        ));
        credited.sort();
        for till in [&mut wallets.till, &mut wallets.labelled] {
            till.receive(None).await?;
            let record = till
                .app()
                .shared_record(&key)
                .await?
                .ok_or("missing shared credit bucket")?;
            let bucket = SettlementBucket::bind(
                record.content.value().ok_or("unreadable bucket")?,
                number,
                till.app().naming_key(),
                &key,
            )?;
            assert_eq!(bucket.marks(), credited.as_slice());
        }
        assert_eq!(
            wallets.rail.all_executions().values().sum::<u64>(),
            2 * credited.len() as u64
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_full_bucket_warns_once_and_a_confirmed_stored_receipt_stays_unsent() -> TestResult {
    use pubky_switch_client::app::{
        Collection as SwitchCollection, OpBatch, RecordTarget, UserConfirmed,
    };
    let Some(world) = World::start(Backend::Scripted)? else {
        return Err("missing scripted switch".into());
    };
    let mut tills = Box::pin(enrolled_tills(&world)).await?;
    let (buyer, request, record) = Box::pin(paid_request(
        &mut tills.till,
        tills.buyer,
        "btc-lightning-bolt11",
        "ln-needs-confirmation",
    ))
    .await?;
    tills.buyer = buyer;
    let proof = tills
        .till
        .app()
        .shared_record(&record)
        .await?
        .ok_or("missing paid request")?;
    let paid = paykit_lib::records::ActivityRecord::read(
        proof.content.value().ok_or("missing paid value")?,
        &tills.till.identity(),
        paykit_lib::records::ActivitySide::Payee,
    )?;
    let signed = paid.proof.as_ref().ok_or("missing signed proof")?;
    let paykit_lib::Body::PaymentProof(body) = signed.body() else {
        return Err("wrong signed role".into());
    };
    let id = tills
        .rail
        .settlement_id(&body.proof)
        .ok_or("missing settlement")?;
    let tag = settlement_tag(tills.till.app().naming_key(), &id);
    let number = settlement_bucket(&tag);
    let mut bytes = Vec::new();
    let mut n = 0u64;
    while bytes.len() < 9000 * paykit_lib::records::SETTLEMENT_MARK_LEN {
        let mut settlement = [0; 12];
        settlement[4..].copy_from_slice(&n.to_be_bytes());
        n += 1;
        if settlement == paykit_lib::derive::mark_settlement_tag(&tag) {
            continue;
        }
        let mark = SettlementMark {
            settlement,
            kind: OccurrenceKind::OneTimeRequest,
            occurrence: [0; 8],
        };
        bytes.extend_from_slice(&mark.to_bytes());
    }
    let collection = Collection::Settlements;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.fixed_policy()),
        &natural_key::settlement_bucket(number),
    );
    let value = serde_json::to_vec(
        &serde_json::json!({"v":1, "marks":pubky_switch_core::enc::b64(&bytes)}),
    )?;
    let put = tills
        .till
        .app_mut()
        .record_op(&target, Some(&value), Vec::new())
        .await?;
    tills
        .till
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(put))
        .await?;
    let before = tills.till.backup().await?;
    for _ in 0..2 {
        let error = tills.till.confirm_payment(&record).await.unwrap_err();
        assert_eq!(
            error.refusal(),
            Some(&paykit_sdk::Refusal::SettlementBucketFull { bucket: number })
        );
    }
    let warnings = tills.till.notices().await?;
    let [warning] = warnings.as_slice() else {
        return Err("expected one bucket warning".into());
    };
    assert_eq!(warning.kind, paykit_sdk::NoticeKind::SettlementBucketFull);
    assert_eq!(
        warning.settlement,
        Some(paykit_sdk::SettlementSubject::Bucket(number))
    );
    let notice_id = warning.id.clone();
    tills.till.acknowledge_notice(&notice_id).await?;
    let stored = tills
        .till
        .confirm_payment_with(
            &record,
            ReceiptDetails::new()
                .with_unmarked_credit_confirmed(UserConfirmed::new())
                .store_only(),
        )
        .await?
        .ok_or("missing stored receipt")?;
    assert!(tills.till.app().delivery(&stored).await?.is_none());
    assert!(tills
        .till
        .app()
        .protocol_scan("paykit", b"ri/")
        .await?
        .is_empty());
    assert!(tills.till.notices().await?.is_empty());
    drop(tills.till);
    let mut restored = Paykit::restore(
        world.app_env(tills.till_store),
        Default::default(),
        &before,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    restored.receive(None).await?;
    assert!(restored
        .notices()
        .await?
        .iter()
        .all(|n| { n.id != notice_id && n.msg_id.as_deref() != Some(stored.as_str()) }));
    assert_eq!(
        restored.confirm_payment(&record).await?,
        Some(stored.clone())
    );
    assert!(restored.app().delivery(&stored).await?.is_none());
    tills.buyer.receive(None).await?;
    assert!(tills
        .buyer
        .receipts(ReceiptsOf::Request(request))
        .await?
        .is_empty());
    assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 1);
    world.close().await?;
    Ok(())
}

/// Follow a real declaration and finish the wallet's reported receive/listing work.
async fn follow_recovered_shop(shop: &mut Paykit) -> TestResult {
    shop.app_mut().refresh_identity().await?;
    shop.app_mut().follow_provider().await?;
    for _ in 0..8 {
        let summary = shop.receive(None).await?;
        if summary.report.more {
            continue;
        }
        if let Some(reason) = summary.records_unlisted {
            assert!(reason.contains("Call receive again"), "{reason}");
            continue;
        }
        assert!(shop
            .app()
            .recovery_info()
            .await?
            .is_some_and(|info| info.steps_done));
        return Ok(());
    }
    Err("the recovered shop did not finish its ordinary listing".into())
}

async fn declare_shop_loss(world: &World, signer: Box<Signer>) -> TestResult {
    let shop = world.identity("shop")?;
    world.lose_switch("https://shop.payment.test");
    let origin = "https://recovered-shop.payment.test";
    world.add_switch(origin, &[&shop]).await?;
    tokio::spawn(Box::pin(async move {
        let mut signer = signer;
        signer
            .declare_provider_loss(
                origin,
                pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
            )
            .await?;
        signer.end_staged_admission().await
    }))
    .await??;
    Ok(())
}

#[tokio::test]
async fn test_recovered_bucket_repairs_retained_marks_without_another_payment() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(repair_recovered_bucket(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn repair_recovered_bucket(world: &World) -> TestResult {
    use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};

    let mut tills = Box::pin(enrolled_tills(world)).await?;
    let [first, second] = transactions_in_one_bucket(tills.till.app().naming_key())?;
    let number = settlement_bucket(&first.1);
    let collection = Collection::Settlements;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.fixed_policy()),
        &natural_key::settlement_bucket(number),
    );
    let key = tills.till.app().record_key(&target)?;
    const ADDRESS: &str = "bcrt1qqyqszqgpqyqszqgpqyqszqgpqyqszqgpvxat9t";
    tills.rail.next(Behaviour::InTransaction(first.0));
    let (buyer, _, record) = Box::pin(paid_request(
        &mut tills.till,
        tills.buyer,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    let receipt = tills
        .till
        .confirm_payment(&record)
        .await?
        .ok_or("first receipt missing")?;
    tills.labelled.receive(None).await?;
    let prior = tills.till.backup().await?;
    let older = tills
        .labelled
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing first mark")?;
    let snapshot = SettlementBucket::bind(
        older.content.value().ok_or("first bucket unreadable")?,
        number,
        tills.labelled.app().naming_key(),
        &key,
    )?;
    assert_eq!(snapshot.marks().len(), 1);

    tills.rail.next(Behaviour::InTransaction(second.0));
    let (_, _, record) = Box::pin(paid_request(
        &mut tills.till,
        buyer,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    let next = tills
        .till
        .confirm_payment(&record)
        .await?
        .ok_or("second receipt missing")?;
    assert_ne!(receipt, next);
    let complete = tills
        .till
        .app_mut()
        .read_record(&target)
        .await?
        .ok_or("missing second mark")?;
    let expected = SettlementBucket::bind(
        complete
            .content
            .value()
            .ok_or("cannot open complete bucket")?,
        number,
        tills.till.app().naming_key(),
        &key,
    )?;
    assert_eq!(expected.marks().len(), 2);
    assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 2);

    // This till selected an older native layer, but its owner layer retains both
    // authenticated marks. The other till can upload only the genuine first write.
    drop(tills.till);
    Box::pin(declare_shop_loss(world, tills.signer)).await?;
    Box::pin(follow_recovered_shop(&mut tills.labelled)).await?;
    let upload = tills
        .labelled
        .app_mut()
        .read_record(&target)
        .await?
        .ok_or("missing uploaded bucket")?;
    assert!(!upload.divergent);
    assert_eq!(upload.content.value(), older.content.value());
    let pubky_switch_core::ops::Condition::RecordCurrent {
        revision,
        deleted: false,
        ..
    } = tills.labelled.app().record_condition(&key).await?
    else {
        return Err("recovery upload lacks its physical application".into());
    };

    let mut app = Paykit::restore(
        world.app_env(tills.till_store.clone()),
        Default::default(),
        &prior,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert_eq!(
        app.app().protocol_scan("paykit", b"sm/").await?.len(),
        2,
        "the older backup must retain both verified marks from this installation"
    );
    let history = app.app().protocol_scan("paykit", b"sb/").await?;
    assert_eq!(
        history.len(),
        1,
        "one bounded record tracks the imported bucket"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&history[0].1)?["d"]["repair"],
        true,
        "the selected older layer must be rechecked on the next wallet read"
    );
    // A crash can follow a native read before Paykit resumes its outstanding repair.
    app.app_mut().refresh_identity().await?;
    app.app_mut().follow_provider().await?;
    let pubky_switch_client::app::RecordRead::Fresh(before) =
        app.app_mut().read_record_fresh(&target).await?
    else {
        return Err("the returning till must receive a current hosted bucket".into());
    };
    assert_eq!(before.writer, upload.writer);
    assert_eq!(before.content.value(), older.content.value());
    assert_eq!(before.version, older.version);
    assert!(
        !before.authenticated,
        "the upload lacks the old service's version proof"
    );
    assert!(!before.divergent && !before.flagged && !before.not_taken && !before.doubtful);
    assert_eq!(
        app.app().record_condition(&key).await?,
        pubky_switch_core::ops::Condition::RecordCurrent {
            key,
            revision,
            deleted: false
        }
    );
    let checked = app.app().protocol_scan("paykit", b"sb/").await?;
    assert_eq!(checked, history);
    let app_id = app.app_id();
    drop(app);
    let mut restored = Paykit::load(
        world.app_env(tills.till_store),
        Default::default(),
        app_id,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    Box::pin(follow_recovered_shop(&mut restored)).await?;
    let repaired = tills
        .labelled
        .app_mut()
        .read_record(&target)
        .await?
        .ok_or("missing repaired bucket")?;
    let actual = SettlementBucket::bind(
        repaired
            .content
            .value()
            .ok_or("repaired bucket unreadable")?,
        number,
        tills.labelled.app().naming_key(),
        &key,
    )?;
    assert_eq!(actual.marks(), expected.marks());
    assert_ne!(repaired.version, upload.version);
    let changed = paykit_sdk::SettlementSubject::Changed {
        bucket: number,
        writer: upload.writer,
        application: revision,
    };
    let notice = restored
        .notices()
        .await?
        .into_iter()
        .find(|n| n.settlement == Some(changed))
        .ok_or("missing authenticated writer of the incomplete bucket")?;
    assert_eq!(notice.kind, paykit_sdk::NoticeKind::SettlementMarksChanged);
    assert_eq!(notice.counterparty, None);
    restored.acknowledge_notice(&notice.id).await?;

    restored.receive(None).await?;
    let again = tills
        .labelled
        .app_mut()
        .read_record(&target)
        .await?
        .ok_or("missing checked bucket")?;
    assert_eq!(again.version, repaired.version);
    assert!(restored
        .notices()
        .await?
        .iter()
        .all(|n| n.settlement != Some(changed)));
    assert_eq!(
        restored.app().protocol_scan("paykit", b"sm/").await?.len(),
        2
    );

    assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 2);
    Ok(())
}

#[derive(Clone, Copy)]
enum SettlementTime {
    Before,
    At,
    After,
}

#[tokio::test]
async fn test_absent_bucket_after_loss_keeps_known_marks_and_uses_the_payment_time() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        for when in [
            SettlementTime::Before,
            SettlementTime::At,
            SettlementTime::After,
        ] {
            let Some(world) = World::start(backend)? else {
                continue;
            };
            let result = Box::pin(credit_after_loss(&world, when)).await;
            world.close().await?;
            result?;
        }
    }
    Ok(())
}

async fn credit_after_loss(world: &World, when: SettlementTime) -> TestResult {
    use pubky_switch_client::app::{
        Collection as SwitchCollection, KeptReason, RecordRead, RecordTarget, UserConfirmed,
    };
    let mut tills = Box::pin(enrolled_tills(world)).await?;
    let [original, distinct] = transactions_in_one_bucket(tills.till.app().naming_key())?;
    let number = settlement_bucket(&original.1);
    let collection = Collection::Settlements;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.fixed_policy()),
        &natural_key::settlement_bucket(number),
    );
    let key = tills.till.app().record_key(&target)?;
    let before = tills.labelled.backup().await?;
    const ADDRESS: &str = "bcrt1qqyqszqgpqyqszqgpqyqszqgpqyqszqgpvxat9t";
    tills.rail.next(Behaviour::InTransaction(original.0));
    let (buyer, _, paid) = Box::pin(paid_request(
        &mut tills.till,
        tills.buyer,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    assert!(tills.till.confirm_payment(&paid).await?.is_some());
    tills.labelled.receive(None).await?;
    assert!(tills.labelled.app().shared_record(&paid).await?.is_none());
    assert_eq!(
        tills
            .labelled
            .app()
            .protocol_scan("paykit", b"sm/")
            .await?
            .len(),
        1
    );
    drop((tills.till, tills.labelled));
    Box::pin(declare_shop_loss(world, tills.signer)).await?;
    // This registered till retains its prior observation locally. Its selected native
    // backup has no bucket to upload, and cannot read the unlabelled sale or receipt.
    let resumed = Paykit::restore(
        world.app_env(tills.labelled_store.clone()),
        Default::default(),
        &before,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    let app_id = resumed.app_id();
    drop(resumed);
    let mut seller = Paykit::load(
        world.app_env(tills.labelled_store),
        Default::default(),
        app_id,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    Box::pin(follow_recovered_shop(&mut seller)).await?;
    assert!(matches!(
        seller.app_mut().read_record_fresh(&target).await?,
        RecordRead::Kept {
            reason: KeptReason::NotFound,
            ..
        }
    ));
    assert_eq!(seller.app().protocol_scan("paykit", b"sm/").await?.len(), 1);
    let details = || ReceiptDetails::new().with_maybe_credited_confirmed(UserConfirmed::new());
    let mut buyer = buyer;
    buyer
        .app_mut()
        .identity_of(&seller.identity(), true)
        .await?;
    seller.app_mut().invite(&buyer.identity()).await?;
    // A second output from the same transaction is never another settlement.
    tills.rail.next(Behaviour::InTransaction(original.0));
    let (buyer, _, same) = Box::pin(paid_request(
        &mut seller,
        buyer,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    assert!(seller
        .confirm_payment_with(&same, details())
        .await?
        .is_none());
    let boundary = seller
        .app()
        .recovery_info()
        .await?
        .ok_or("missing loss")?
        .loss
        .uncertain_until_ms;
    let paid_at = match when {
        SettlementTime::Before => boundary - 1,
        SettlementTime::At => boundary,
        SettlementTime::After => boundary + 1,
    };
    world.advance_ms(
        paid_at
            .checked_sub(world.now_ms())
            .ok_or("unexpected recovery clock")?,
    );
    tills.rail.next(Behaviour::InTransaction(distinct.0));
    let (mut buyer, request, occurrence) = Box::pin(paid_request(
        &mut seller,
        buyer,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    let receipt = if matches!(when, SettlementTime::After) {
        seller.confirm_payment(&occurrence).await?
    } else {
        let refusal = seller.confirm_payment(&occurrence).await.unwrap_err();
        assert!(matches!(
            refusal.refusal(),
            Some(paykit_sdk::Refusal::MaybeCredited)
        ));
        seller.confirm_payment_with(&occurrence, details()).await?
    }
    .ok_or("eligible distinct payment missing a receipt")?;
    buyer.receive(None).await?;
    let receipts = buyer.receipts(ReceiptsOf::Request(request)).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, receipt);
    let current = seller
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing rebuilt bucket")?;
    let bucket = SettlementBucket::bind(
        current.content.value().ok_or("unreadable rebuilt bucket")?,
        number,
        seller.app().naming_key(),
        &key,
    )?;
    assert_eq!(bucket.marks().len(), 2);
    assert!(bucket
        .marks()
        .iter()
        .any(|mark| mark.settlement == paykit_lib::derive::mark_settlement_tag(&original.1)));
    assert!(bucket
        .marks()
        .iter()
        .any(|mark| mark.settlement == paykit_lib::derive::mark_settlement_tag(&distinct.1)));
    assert_eq!(seller.confirm_payment(&occurrence).await?, Some(receipt));
    assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 3);
    Ok(())
}

#[tokio::test]
async fn test_absent_settlement_bucket_without_a_loss_keeps_the_wallet_stopped() -> TestResult {
    use pubky_switch_client::app::{
        Alarm, AppError, Collection as SwitchCollection, KeptReason, RecordRead, RecordTarget,
    };

    let world = World::start(Backend::Scripted)?.ok_or("missing scripted switch")?;
    let mut tills = Box::pin(enrolled_tills(&world)).await?;
    let [credited, pending] = transactions_in_one_bucket(tills.till.app().naming_key())?;
    let number = settlement_bucket(&credited.1);
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::Settlements.name(),
            Collection::Settlements.fixed_policy(),
        ),
        &natural_key::settlement_bucket(number),
    );
    let key = tills.till.app().record_key(&target)?;
    const ADDRESS: &str = "bcrt1qqyqszqgpqyqszqgpqyqszqgpqyqszqgpvxat9t";

    // This backup has the pending proof but predates the other till's first credit.
    tills.rail.next(Behaviour::InTransaction(pending.0));
    let (mut buyer, pending_request, pending_record) = Box::pin(paid_request(
        &mut tills.labelled,
        tills.buyer,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    let prior = tills.labelled.backup().await?;
    tills.rail.next(Behaviour::InTransaction(credited.0));
    let (_, _, first) = Box::pin(paid_request(
        &mut tills.till,
        tills.other,
        "btc-bitcoin-p2wpkh",
        ADDRESS,
    ))
    .await?;
    assert!(tills.till.confirm_payment(&first).await?.is_some());
    tills.labelled.receive(None).await?;
    let known = tills.labelled.app().protocol_scan("paykit", b"sm/").await?;
    assert_eq!(known.len(), 1);
    drop((tills.till, tills.labelled));

    // The faulty test service still hosts this same generation, but its answer omits the bucket.
    world
        .drop_record("https://shop.payment.test", &world.identity("shop")?, &key)
        .await?;
    let mut shop = Paykit::restore(
        world.app_env(tills.labelled_store.clone()),
        Default::default(),
        &prior,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert!(shop.app().recovery_info().await?.is_none());
    assert!(shop.app().shared_record(&key).await?.is_none());
    assert_eq!(shop.app().protocol_scan("paykit", b"sm/").await?, known);
    assert!(matches!(
        shop.app_mut().read_record_fresh(&target).await?,
        RecordRead::Kept {
            record: None,
            reason: KeptReason::NotFound
        }
    ));
    assert!(shop.app().alarm().is_none());

    let refusal = shop.confirm_payment(&pending_record).await.unwrap_err();
    assert!(
        matches!(refusal, paykit_sdk::PaykitError::Client(AppError::Alarm(Alarm::ProtocolRollback { ref protocol, ref reason }))
        if protocol == "paykit" && reason.contains("permanent settlement bucket")),
        "{refusal:?}"
    );
    let alarm = shop
        .app()
        .alarm()
        .cloned()
        .ok_or("missing rollback alarm")?;
    let id = shop.app_id();
    drop(shop);
    let mut shop = Paykit::load(
        world.app_env(tills.labelled_store),
        Default::default(),
        id,
        tills.rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    assert_eq!(shop.app().alarm(), Some(&alarm));
    assert!(shop.confirm_payment(&pending_record).await.is_err());
    let held = shop
        .app()
        .shared_record(&pending_record)
        .await?
        .ok_or("missing pending proof")?;
    let sale = paykit_lib::records::ActivityRecord::read(
        held.content.value().ok_or("unreadable pending sale")?,
        &shop.identity(),
        paykit_lib::records::ActivitySide::Payee,
    )?;
    assert!(sale.proof.is_some() && sale.receipt.is_none());
    buyer.receive(None).await?;
    assert!(buyer
        .receipts(ReceiptsOf::Request(pending_request))
        .await?
        .is_empty());
    assert_eq!(shop.app().protocol_scan("paykit", b"sm/").await?, known);
    assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 2);
    world.close().await?;
    Ok(())
}

#[tokio::test]
async fn test_restored_receipt_delivery_after_a_recovered_request_was_cancelled() -> TestResult {
    receipt_after_recovery(ReceiptHistory::Original).await
}

#[tokio::test]
async fn test_restored_receipt_waits_when_its_original_application_was_unavailable() -> TestResult {
    receipt_after_recovery(ReceiptHistory::Unavailable).await
}

#[tokio::test]
async fn test_restored_receipt_at_the_last_millisecond_of_the_writers_standing() -> TestResult {
    receipt_after_recovery(ReceiptHistory::AtRemoval).await
}

#[tokio::test]
async fn test_restored_receipt_after_the_writers_removal_cutoff_is_held() -> TestResult {
    receipt_after_recovery(ReceiptHistory::AfterRemoval).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiptHistory {
    Original,
    Unavailable,
    AtRemoval,
    AfterRemoval,
}

async fn receipt_after_recovery(history: ReceiptHistory) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(restored_receipt_delivery_after_cancellation(
            &world, history,
        ))
        .await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn restored_receipt_delivery_after_cancellation(
    world: &World,
    history: ReceiptHistory,
) -> TestResult {
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    use paykit_sdk::NoticeKind;

    let mut tills = Box::pin(enrolled_tills(world)).await?;
    let (mut peer, signer) = Box::pin(join(
        world,
        *tills.signer,
        "shop",
        "other-shop",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &tills.rail,
    ))
    .await?;
    let (mut buyer, request, key) = Box::pin(paid_request(
        &mut tills.till,
        tills.buyer,
        "btc-lightning-bolt11",
        "ln-receipt-before-shop-recovery",
    ))
    .await?;
    let shop_id = tills.till.app_id();
    world.lose_answer("POST", "/ops");
    if history == ReceiptHistory::Unavailable {
        world.unreachable(
            "GET",
            &format!("/records/{}", pubky_switch_core::enc::b64(&key)),
        );
    }
    let kept = tills
        .till
        .confirm_payment_with(&key, ReceiptDetails::new())
        .await
        .unwrap_err()
        .kept_batch()
        .ok_or("missing unanswered receipt batch")?;
    assert_eq!(world.faults_left(), 0);
    let original = tills
        .till
        .app()
        .queued_messages("paykit")
        .await?
        .into_iter()
        .find(|message| message.kept_batch == Some(kept))
        .ok_or("missing original receipt intent")?;
    assert_eq!(original.to, buyer.identity());
    assert_eq!(original.kind, paykit_lib::Kind::PaymentReceipt.as_str());
    let backup = tills.till.backup().await?;
    drop(tills.till);
    peer.receive(None).await?;
    let first = peer
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing credit")?;
    assert!(first.authenticated);
    let receipt = |value: &[u8]| -> TestResult<_> {
        ActivityRecord::read(value, &peer.identity(), ActivitySide::Payee)?
            .receipt
            .ok_or_else(|| "missing receipt".into())
    };
    let issued = receipt(first.content.value().ok_or("unreadable credit")?)?;
    assert_eq!(issued.hash(), original.hash);
    assert_eq!(issued.message().msg_id, original.msg_id);
    assert_eq!(issued.cert().app_id(), shop_id);
    if matches!(
        history,
        ReceiptHistory::AtRemoval | ReceiptHistory::AfterRemoval
    ) {
        let written = first.written_at_ms.ok_or("undated issued receipt")?;
        let cutoff = if history == ReceiptHistory::AtRemoval {
            written
        } else {
            written - 1
        };
        let mut signer = signer;
        let historical = Box::pin(signer.revoke(
            shop_id,
            Some(cutoff),
            pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
        ))
        .await?;
        assert_eq!(
            historical.standing(&shop_id, &issued.cert().hash()),
            pubky_switch_core::records::Standing::Removed {
                distrust_from: cutoff
            }
        );
        Box::pin(check_receipt_at_removal(
            world,
            &backup,
            &historical,
            issued.cert(),
            &original,
            &tills.rail,
            cutoff == written,
        ))
        .await?;
        assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 1);
        return Ok(());
    }
    Box::pin(declare_shop_loss(world, Box::new(signer))).await?;
    Box::pin(follow_recovered_shop(&mut peer)).await?;
    peer.cancel_sent_request(&key, None).await?;
    let successor = peer
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing cancellation")?;
    assert!(successor.authenticated && successor.version.generation > first.version.generation);
    assert_eq!(successor.writer, peer.app_id());
    let current = ActivityRecord::read(
        successor.content.value().ok_or("unreadable cancellation")?,
        &peer.identity(),
        ActivitySide::Payee,
    )?;
    assert!(current.cancellation.is_some());
    assert_eq!(
        current
            .receipt
            .ok_or("cancellation lost the receipt")?
            .hash(),
        original.hash
    );
    let store = Arc::new(MemoryStore::new());
    let mut restored = Box::pin(Paykit::restore(
        world.app_env(store.clone()),
        Default::default(),
        &backup,
        tills.rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    Box::pin(follow_recovered_shop(&mut restored)).await?;
    let unresolved: Vec<_> = restored
        .notices()
        .await?
        .into_iter()
        .filter(|n| n.kind == NoticeKind::PossiblyDelivered)
        .collect();
    let markers = restored.app().protocol_scan("paykit", b"ir/").await?;
    let notice_id = if history != ReceiptHistory::Unavailable {
        let [notice] = unresolved.as_slice() else {
            return Err("expected one uncertain receipt delivery".into());
        };
        assert_eq!(notice.counterparty, Some(buyer.identity()));
        assert_eq!(notice.msg_id.as_deref(), Some(original.msg_id.as_str()));
        assert_eq!(notice.message_hash, Some(original.hash));
        assert!(markers.is_empty());
        Some(notice.id.clone())
    } else {
        assert!(unresolved.is_empty());
        let [(_, id)] = markers.as_slice() else {
            return Err("expected the unanswered original marker".into());
        };
        assert_eq!(id.as_slice(), kept);
        None
    };
    drop(restored);
    let mut reopened = Box::pin(Paykit::load(
        world.app_env(store),
        Default::default(),
        shop_id,
        tills.rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    reopened.receive(None).await?;
    if let Some(notice_id) = notice_id {
        assert_eq!(
            reopened
                .notices()
                .await?
                .iter()
                .filter(|n| n.id == notice_id)
                .count(),
            1
        );
        reopened.acknowledge_notice(&notice_id).await?;
        reopened.receive(None).await?;
        assert!(reopened.notices().await?.iter().all(|n| n.id != notice_id));
    } else {
        assert!(reopened
            .notices()
            .await?
            .iter()
            .all(|n| n.kind != NoticeKind::PossiblyDelivered));
        assert_eq!(
            reopened.app().protocol_scan("paykit", b"ir/").await?,
            markers
        );
    }
    assert!(reopened.app().delivery(&original.msg_id).await?.is_none());
    buyer.receive(None).await?;
    assert!(buyer
        .receipts(ReceiptsOf::Request(request))
        .await?
        .is_empty());
    assert_eq!(tills.rail.all_executions().values().sum::<u64>(), 1);
    Ok(())
}

async fn check_receipt_at_removal(
    world: &World,
    backup: &paykit_sdk::PaykitBackup,
    historical: &pubky_switch_core::records::IdentityDoc,
    cert: &pubky_switch_core::records::AppCert,
    original: &pubky_switch_client::app::QueuedMessage,
    rail: &Arc<FakeRail>,
    valid: bool,
) -> TestResult {
    assert!(paykit_lib::scopes::certificate_in_force_at_write(
        historical,
        cert,
        cert.issued_at()
    ));
    assert!(!paykit_lib::scopes::certificate_in_force_at_write(
        historical,
        cert,
        cert.issued_at() - 1
    ));
    let selected = receipt_backup_with_historical_identity(backup, historical)?;
    let store = Arc::new(MemoryStore::new());
    let restored = Box::pin(Paykit::restore(
        world.app_env(store.clone()),
        Default::default(),
        &selected,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await;
    if valid {
        let restored = restored?;
        let markers = restored.app().protocol_scan("paykit", b"ir/").await?;
        let [(key, marker)] = markers.as_slice() else {
            return Err("missing verified imported receipt".into());
        };
        let application: serde_json::Value = serde_json::from_slice(marker)?;
        assert_eq!(
            application["batch"],
            serde_json::to_value(original.kept_batch.ok_or("missing original batch")?)?
        );
        let receipt = format!(
            "/{}/{}",
            pubky_switch_core::enc::b64(&original.hash),
            original.msg_id
        );
        assert!(key.ends_with(receipt.as_bytes()));
        assert_eq!(
            application["identity"],
            serde_json::to_value(historical.signed())?
        );
    } else {
        assert!(
            matches!(
                restored,
                Err(paykit_sdk::PaykitError::Client(
                    pubky_switch_client::app::AppError::InvalidData(_)
                ))
            ),
            "the later write has no valid historical authorization"
        );
        let namespace = pubky_switch_client::store::client_namespace(&cert.app_id());
        assert!(pubky_switch_client::store::StateStore::scan(
            store.as_ref(),
            &namespace,
            b"",
            None,
            1
        )?
        .is_empty());
    }
    Ok(())
}

/// Select a later, identity-signed document from the same historical service generation.
/// The original signed operation, certificate, service proof and receipt stay byte-identical.
fn receipt_backup_with_historical_identity(
    backup: &paykit_sdk::PaykitBackup,
    historical: &pubky_switch_core::records::IdentityDoc,
) -> TestResult<paykit_sdk::PaykitBackup> {
    use pubky_switch_client::app::{decode_local_state, encode_local_state};
    use pubky_switch_core::enc::{b64, unb64};
    use pubky_switch_core::records::IdentityDoc;

    let mut selected: serde_json::Value = serde_json::from_slice(&backup.to_bytes()?)?;
    let layer = &mut selected["layers"][0];
    assert_eq!(layer["name"], "paykit");
    let raw = unb64(layer["data"].as_str().ok_or("missing owner layer")?)?;
    let mut entries = decode_local_state(&raw).ok_or("unreadable owner layer")?;
    let (_, value) = entries
        .iter_mut()
        .find(|(k, _)| k.starts_with(b"ri/"))
        .ok_or("missing original receipt intent")?;
    let mut application: serde_json::Value = serde_json::from_slice(value)?;
    let previous = IdentityDoc::verify(
        &serde_json::from_value(application["identity"].clone())?,
        &historical.identity(),
    )?;
    previous.check_successor(historical)?;
    assert_eq!(previous.generation(), historical.generation());
    application["identity"] = serde_json::to_value(historical.signed())?;
    *value = serde_json::to_vec(&application)?;
    layer["data"] = b64(&encode_local_state(&entries)).into();
    Ok(paykit_sdk::PaykitBackup::from_bytes(&serde_json::to_vec(
        &selected,
    )?)?)
}
