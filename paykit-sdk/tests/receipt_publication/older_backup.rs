use super::*;
use paykit_sdk::PaykitBackup;
use pubky_switch_client::app::{KeptBatchDrop, KeptBatchStatus, RecordRead};
use pubky_switch_client::client::Batch;
use pubky_switch_core::enc::{serde_b64, serde_b64_vec};
use serde::Deserialize;

/// The two restore duties in the shop's unmodified serialized backup.
#[derive(Deserialize)]
struct Publications {
    put_backs: Vec<Hold>,
    record_settlements: Vec<Submission>,
}

#[derive(Deserialize)]
struct Hold {
    observation: Observation,
    protects: bool,
    pending: Option<Submission>,
}

#[derive(Deserialize)]
struct Observation {
    #[serde(with = "serde_b64")]
    key: [u8; 32],
    seq: u64,
    delete: Deletion,
}

#[derive(Deserialize)]
struct Deletion {
    #[serde(with = "serde_b64")]
    item_hash: [u8; 32],
}

#[derive(Deserialize)]
struct Submission {
    #[serde(with = "serde_b64_vec")]
    body: Vec<u8>,
    kept_ms: Option<u64>,
    record_deletion: Option<Deletion>,
}

fn publications(backup: &PaykitBackup) -> TestResult<Publications> {
    Ok(serde_json::from_slice(&backup.to_bytes()?)?)
}

#[tokio::test]
async fn test_older_shop_backup_keeps_the_earlier_hold_and_original_submission() -> TestResult {
    with_issued_sale(older_shop_backup).await
}

async fn older_shop_backup(world: &World, sale: IssuedSale) -> TestResult {
    let IssuedSale {
        mut buyer,
        mut shop,
        mut till,
        store,
        rail,
        request,
        key,
        target,
        observer,
        seller,
        mut proof_only,
        receipt,
        ..
    } = sale;
    let identity = shop.identity();
    let cert = shop.app().cert().clone();

    let (mut peer, _) = join(
        world,
        seller,
        "shop",
        "repairing-till",
        AppScopes::new().request().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    peer.app_mut().read_record_fresh(&target).await?;
    peer.receive(None).await?;
    let prior = peer
        .app()
        .shared_record(&key)
        .await?
        .ok_or("the other till has no sale")?;
    let prior_receipt = ActivityRecord::read(
        prior.content.value().ok_or("unreadable sale")?,
        &identity,
        ActivitySide::Payee,
    )?
    .receipt
    .ok_or("the other till has no receipt")?;
    assert_eq!(prior_receipt.hash(), receipt.hash());

    // The shop was offline while the other till created and deleted a different request.
    let earlier_request = till
        .send_request(
            buyer.identity(),
            invoice("btc-lightning-bolt11", "ln-other")?,
        )
        .await?
        .request
        .ok_or("missing independent request")?;
    let earlier_target = RecordTarget::new(
        target.collection.clone(),
        &natural_key::request_record(&identity, &earlier_request.request_hash),
    );
    let earlier = till
        .app_mut()
        .delete_record(&earlier_target, vec![])
        .await?;
    assert_ne!(earlier.key, key);
    shop.receive(None).await?;
    shop.app_mut().flush_cursor().await?;
    let held_cursor = service_cursor(&observer).await?;
    assert_eq!(held_cursor + 1, earlier.version.version);
    let initial = publications(&shop.backup().await?)?;
    assert_eq!(initial.put_backs.len(), 1);
    assert!(initial.put_backs[0].protects);
    assert_eq!(initial.put_backs[0].observation.key, earlier.key);
    assert!(initial.put_backs[0].pending.is_none());

    let deleted = till.app_mut().delete_record(&target, vec![]).await?;
    let deletion = record_op(&till, key, till.app().cert()).await?;
    let deletion_ref = ApplicationRef {
        generation: deleted.version.generation,
        seq: deleted.version.version,
        item_hash: deletion.item_hash()?,
    };
    shop.app_mut().read_record_fresh(&target).await?;
    peer.app_mut().read_record_fresh(&target).await?;
    proof_only["proof"]["cert"]["shipping_note"] = "delivered".into();
    let eager = till
        .app_mut()
        .put_record(
            &target,
            &serde_json::to_vec(&proof_only)?,
            vec![Condition::RecordCurrent {
                key,
                revision: deletion_ref,
                deleted: true,
            }],
        )
        .await?;
    let eager_ref = ApplicationRef {
        generation: eager.version.generation,
        seq: eager.version.version,
        item_hash: record_op(&till, key, till.app().cert())
            .await?
            .item_hash()?,
    };
    world.unreachable("POST", "/ops");
    // The feed delivers the proof-only write, then the next receive submits the repair.
    shop.receive(None).await?;
    assert_eq!(world.faults_left(), 1);
    shop.receive(None).await?;
    assert_eq!(world.faults_left(), 0);
    let older = shop.backup().await?;
    let duties = publications(&older)?;
    assert_eq!(duties.put_backs.len(), 2);
    assert!(duties.record_settlements.is_empty());
    let held = duties
        .put_backs
        .iter()
        .find(|h| h.observation.key == key)
        .ok_or("missing receipt publication")?;
    assert!(held.protects);
    assert_eq!(held.observation.seq, deleted.version.version);
    assert_eq!(held.observation.delete.item_hash, deletion_ref.item_hash);
    let sent = held
        .pending
        .as_ref()
        .ok_or("receipt comparison was never sent")?;
    assert!(sent.kept_ms.is_some());
    let original_batch = Batch::from_body(sent.body.clone())?;
    let id = original_batch.id().ok_or("original batch has no ID")?;
    let [op] = original_batch.ops() else {
        return Err("expected one conditional publication".into());
    };
    let signed = SignedOp::verify_with_cert(op.signed(), &cert, &identity)?;
    let comparison = Condition::RecordCurrent {
        key,
        revision: eager_ref,
        deleted: false,
    };
    assert_eq!(
        signed.body().conditions.as_slice(),
        std::slice::from_ref(&comparison)
    );
    assert!(
        matches!(&signed.body().kind, OpKind::RecordPut(p) if p.key == key && p.expected_version == eager.version && p.policy == target.collection.policy)
    );

    // The other receipt holder publishes through its normal wallet receive.
    world.advance_ms(1000);
    peer.receive(None).await?;
    peer.receive(None).await?;
    let complete = peer
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing peer publication")?;
    assert!(complete.version.version > eager.version.version);
    assert_eq!(complete.writer, peer.app_id());
    let published = record_op(&peer, key, peer.app().cert()).await?;
    assert_eq!(published.body().conditions, [comparison]);
    let RecordRead::Fresh(current) = shop.app_mut().read_record_fresh(&target).await? else {
        return Err("missing complete repair".into());
    };
    assert_eq!(current.version, complete.version);
    let kept = KeptBatchStatus::Kept {
        wire_batch_id: id,
        sent_at_ms: sent.kept_ms,
    };
    assert_eq!(shop.app().kept_batch(id).await?, Some(kept.clone()));
    drop(shop);
    let mut shop = Box::pin(Paykit::restore(
        world.app_env(store),
        Default::default(),
        &older,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    assert_eq!(shop.app().kept_batch(id).await?, Some(kept));
    let after = publications(&shop.backup().await?)?;
    let [first] = after.put_backs.as_slice() else {
        return Err("expected only the earlier deletion after restore".into());
    };
    assert!(first.protects && first.pending.is_none());
    assert_eq!(first.observation.key, earlier.key);
    assert_eq!(first.observation.seq, earlier.version.version);
    assert_eq!(
        first.observation.delete.item_hash,
        initial.put_backs[0].observation.delete.item_hash
    );
    let [retry] = after.record_settlements.as_slice() else {
        return Err("expected only the original submission after restore".into());
    };
    assert_eq!(retry.body, sent.body);
    assert_eq!(retry.kept_ms, sent.kept_ms);
    assert_eq!(
        retry.record_deletion.as_ref().map(|d| d.item_hash),
        Some(deletion_ref.item_hash)
    );
    assert_eq!(service_cursor(&observer).await?, held_cursor);

    shop.receive(None).await?;
    let Some(KeptBatchStatus::Dropped {
        reason: KeptBatchDrop::Refused { code },
        ..
    }) = shop.app().kept_batch(id).await?
    else {
        return Err("missing refusal of the original signed comparison".into());
    };
    assert_eq!(code, "conflict");
    world.advance_ms(DEFAULT_CURSOR_DEFER_MS + 1);
    shop.receive(None).await?;
    assert_eq!(service_cursor(&observer).await?, held_cursor);
    assert_eq!(
        observer.record(&key).await?.view.version,
        complete.version.version
    );
    let RecordRead::Fresh(shared) = shop.app_mut().read_record_fresh(&target).await? else {
        return Err("missing settled receipt".into());
    };
    assert_eq!(shared.version, complete.version);
    assert_eq!(shared.writer, peer.app_id());
    let bytes = shared.content.value().ok_or("unreadable settled receipt")?;
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    assert_eq!(value["proof"]["cert"]["merchant_note"], "collected");
    assert_eq!(value["proof"]["cert"]["shipping_note"], "delivered");
    let accepted = ActivityRecord::read(bytes, &identity, ActivitySide::Payee)?
        .receipt
        .ok_or("receipt missing")?;
    assert_eq!(accepted.hash(), receipt.hash());
    let retained = shop.retained_payee_receipts().await?;
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].receipt.hash(), receipt.hash());
    buyer.receive(None).await?;
    let received = buyer.receipts(ReceiptsOf::Request(request)).await?;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].msg_id, receipt.message().msg_id);
    let paid = BTreeMap::from([(received[0].attempt_id, 1)]);
    assert_eq!(rail.all_pay_calls(), paid);
    assert_eq!(rail.all_executions(), paid);
    Ok(())
}
