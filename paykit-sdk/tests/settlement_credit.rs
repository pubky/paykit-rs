//! Independent tills confirm a shared invoice settlement through the ordinary wallet.
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
    let mut signer = Signer::create(env, world.keyring(&identity)?, origin).await?;
    signer.resume().await?;
    Ok(signer)
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
    invoice: &str,
) -> TestResult<(Paykit, RequestRef, [u8; 32])> {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
        PaymentEndpointPayload::new(invoice)?,
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
        assert!(matches!(payment, PayOutcome::Paid { .. }));
        Ok::<_, paykit_sdk::PaykitError>(buyer)
    }))
    .await??;
    shop.receive(None).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("request missing")?;
    assert_eq!(sale.status, PayeeStatus::ProofReceived);
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
    let (labelled, _) = join(
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
    } = Box::pin(enrolled_tills(world)).await?;
    let before_credit = till.backup().await?;
    // Both authorized tills see the same legitimate purchase in the shop's labelled activity.
    let (mut buyer, winner, first_record) =
        Box::pin(paid_request(&mut labelled, buyer, "ln-one-invoice")).await?;
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

    let (mut other, second, second_record) =
        Box::pin(paid_request(&mut reopened, other, "ln-independent-paid")).await?;
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

/// Two independent transactions in the same bucket. Each has two payments and one
/// method settlement ID under Paykit wire 1.5.
async fn two_transactions(world: &World) -> TestResult {
    let mut wallets = Box::pin(enrolled_tills(world)).await?;
    let naming = wallets.till.app().naming_key();
    let transaction = |number: u64| {
        let mut id = [0; 32];
        id[24..].copy_from_slice(&number.to_be_bytes());
        let text = format!("{number:064x}");
        let tag = settlement_tag(naming, text.as_bytes());
        (id, tag)
    };
    let first = transaction(1);
    let number = settlement_bucket(&first.1);
    let second = (2..100_000)
        .map(transaction)
        .find(|(_, tag)| settlement_bucket(tag) == number)
        .ok_or("second test transaction must be in the same bucket")?;
    let key = SettlementBucket::record_key(number, naming)?;
    assert!(wallets.till.app().shared_record(&key).await?.is_none());
    let mut credited = Vec::new();
    for (txid, tag) in [first, second] {
        wallets.rail.next(Behaviour::InTransaction(txid));
        let (buyer, plain, plain_key) = Box::pin(paid_request(
            &mut wallets.till,
            wallets.buyer,
            "ln-batch-plain",
        ))
        .await?;
        wallets.rail.next(Behaviour::InTransaction(txid));
        wallets.buyer = buyer;
        let (other, label, label_key) = Box::pin(paid_request(
            &mut wallets.labelled,
            wallets.other,
            "ln-batch-label",
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
    assert!(restored.notices().await?.is_empty());
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
