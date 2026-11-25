use super::*;

#[tokio::test]
async fn test_receipt_cursor_readers_verify_the_repair_of_a_marked_one_time_receipt() -> TestResult
{
    with_issued_sale(read_repaired_marker).await
}

async fn read_repaired_marker(world: &World, sale: IssuedSale) -> TestResult {
    let IssuedSale {
        mut shop,
        seller_wallet: mut other_wallet,
        mut buyer,
        seller,
        rail,
        key,
        target,
        observer,
        receipt,
        mut original,
        request,
        ..
    } = sale;
    let identity = shop.identity();
    let scopes = || AppScopes::new().request().build();
    let (mut witness, seller) = join(
        world,
        seller,
        "shop",
        "witness",
        scopes()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    let witness_client = shop_client(world, &seller, &witness, "witness")?;
    witness.app_mut().read_record_fresh(&target).await?;
    witness.receive(None).await?;
    let prior = witness
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing original sale")?;
    assert!(prior.authenticated && !prior.flagged && !prior.not_taken);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            prior.content.value().ok_or("unreadable sale")?
        )?,
        original
    );

    let deleted = other_wallet
        .app_mut()
        .delete_record(&target, vec![])
        .await?;
    let delete = record_op(&other_wallet, key, other_wallet.app().cert()).await?;
    assert!(matches!(&delete.body().kind, OpKind::RecordPut(p)
        if p.key == key && p.expected_version == prior.version && p.value_sealed.looks_like_delete()));
    let delete_ref = ApplicationRef {
        generation: deleted.version.generation,
        seq: deleted.version.version,
        item_hash: delete.item_hash()?,
    };
    for wallet in [&mut shop, &mut witness] {
        wallet.app_mut().read_record_fresh(&target).await?;
        let notices = wallet.payee_deletion_notices().await?;
        assert!(notices
            .iter()
            .any(|n| n.delete == delete_ref.item_hash && n.writer == Some(other_wallet.app_id())));
    }
    let mut eager = original.clone();
    eager["lost_to_delete"] = pubky_dispatch_core::enc::b64(&delete_ref.item_hash).into();
    let cert = eager["proof"]["cert"]
        .as_object_mut()
        .ok_or("missing proof certificate")?;
    assert_eq!(cert.remove("merchant_note"), Some("collected".into()));
    cert.insert("shipping_note".into(), "delivered".into());
    let answers_delete = Condition::RecordCurrent {
        key,
        revision: delete_ref,
        deleted: true,
    };
    let marked = other_wallet
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&eager)?, vec![answers_delete])
        .await?;
    let signed = record_op(&other_wallet, key, other_wallet.app().cert()).await?;
    assert_eq!(signed.body().conditions, [answers_delete]);
    assert!(
        matches!(&signed.body().kind, OpKind::RecordPut(p) if p.key == key && p.expected_version == deleted.version && p.policy == target.collection.policy)
    );
    let marked_ref = ApplicationRef {
        generation: marked.version.generation,
        seq: marked.version.version,
        item_hash: signed.item_hash()?,
    };

    let (mut late, _) = join(
        world,
        seller,
        "shop",
        "late",
        scopes()?,
        Arc::new(MemoryStore::new()),
        &rail,
    )
    .await?;
    for wallet in [&mut witness, &mut late] {
        wallet.app_mut().read_record_fresh(&target).await?;
        let held = wallet
            .app()
            .shared_record(&key)
            .await?
            .ok_or("missing marked receipt")?;
        assert!(held.authenticated && !held.flagged && !held.not_taken);
        assert_eq!(held.version, marked.version);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                held.content.value().ok_or("unreadable marked receipt")?
            )?,
            eager
        );
    }
    assert!(late.retained_payee_receipts().await?.is_empty());
    assert_eq!(
        observer.record(&key).await?.view.version,
        marked.version.version
    );
    assert!(service_cursor(&observer).await? < delete_ref.seq);
    assert!(service_cursor(&witness_client).await? < delete_ref.seq);

    // The issuing wallet reads the marked record, then submits its conditional repair.
    shop.receive(None).await?;
    shop.receive(None).await?;
    let repaired = shop
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing published receipt")?;
    assert!(repaired.authenticated && !repaired.flagged && !repaired.not_taken);
    assert_eq!(repaired.writer, shop.app_id());
    assert!(repaired.version.version > marked_ref.seq);
    let signed = record_op(&shop, key, shop.app().cert()).await?;
    assert_eq!(
        signed.body().conditions,
        [Condition::RecordCurrent {
            key,
            revision: marked_ref,
            deleted: false
        }]
    );
    assert!(
        matches!(&signed.body().kind, OpKind::RecordPut(p) if p.key == key && p.expected_version == marked.version && p.policy == target.collection.policy)
    );
    original["proof"]["cert"]["shipping_note"] = "delivered".into();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            repaired.content.value().ok_or("unreadable repair")?
        )?,
        original
    );

    witness.app_mut().read_record_fresh(&target).await?;
    witness.receive(None).await?;
    witness.app_mut().flush_cursor().await?;
    let proved = witness
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing proved repair")?;
    assert!(proved.authenticated && !proved.flagged && !proved.not_taken);
    assert_eq!(proved.version, repaired.version);
    assert_eq!(proved.writer, shop.app_id());
    assert_eq!(proved.content, repaired.content);
    assert!(service_cursor(&witness_client).await? >= delete_ref.seq);
    let retained = witness.retained_payee_receipts().await?;
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].receipt.hash(), receipt.hash());
    assert!(matches!(
        witness.confirm_payment(&key).await.unwrap_err().refusal(),
        Some(Refusal::CreditHistoryLost)
    ));

    late.app_mut().read_record_fresh(&target).await?;
    let unproved = late
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing earlier restriction")?;
    assert!(unproved.flagged);
    assert_eq!(unproved.version, marked.version);
    let held = ActivityRecord::read(
        unproved.content.value().ok_or("unreadable restriction")?,
        &identity,
        ActivitySide::Payee,
    )?;
    assert_eq!(held.lost_to_delete, Some(delete_ref.item_hash));
    assert_eq!(
        held.receipt.ok_or("missing signed receipt")?.hash(),
        receipt.hash()
    );
    assert_eq!(
        observer.record(&key).await?.view.version,
        repaired.version.version
    );
    world.advance_ms(DEFAULT_CURSOR_DEFER_MS + 1);
    shop.receive(None).await?;
    shop.app_mut().flush_cursor().await?;
    assert!(service_cursor(&observer).await? >= repaired.version.version);
    buyer.receive(None).await?;
    let listed = buyer.receipts(ReceiptsOf::Request(request)).await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].msg_id, receipt.message().msg_id);
    let once = BTreeMap::from([(listed[0].attempt_id, 1)]);
    assert_eq!(rail.all_pay_calls(), once);
    assert_eq!(rail.all_executions(), once);
    Ok(())
}
