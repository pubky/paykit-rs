//! A receipt settles another wallet's requested attempt after the signed proof is recovered.

use super::*;
use paykit_lib::records::StoredMessage;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::PaykitBackup;
use pubky_switch_client::app::{Collection, RecordTarget};
use pubky_switch_core::claims::{Basis, ClaimStatus, EntryKind, Outcome};
use pubky_switch_core::ops::{OpKind, SignedOp};

#[tokio::test]
async fn test_foreign_requested_receipt_recovered_on_scripted_switch() -> TestResult {
    Box::pin(recovered_receipt(Backend::Scripted)).await
}

#[tokio::test]
async fn test_foreign_requested_receipt_recovered_on_postgres() -> TestResult {
    Box::pin(recovered_receipt(Backend::Service)).await
}

async fn recovered_receipt(backend: Backend) -> TestResult {
    for remove_writer in [false, true] {
        let Some(world) = World::start(backend)? else {
            return Ok(());
        };
        let result = Box::pin(receipt_after_loss(&world, remove_writer)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn payer_wallet(
    w: &World,
    signer: &mut Signer,
    name: &str,
    rail: &Arc<FakeRail>,
) -> TestResult<Box<Paykit>> {
    let scopes = AppScopes::new().pay().manage_contacts().build()?;
    Ok(Box::new(
        Box::pin(join(
            w,
            signer,
            "payer",
            name,
            scopes,
            Arc::new(MemoryStore::new()),
            rail,
        ))
        .await?,
    ))
}

async fn restore(
    w: &World,
    backup: &PaykitBackup,
    rail: &Arc<FakeRail>,
) -> TestResult<Box<Paykit>> {
    let backup = PaykitBackup::from_bytes(&backup.to_bytes()?)?;
    Ok(Box::new(
        Box::pin(Paykit::restore(
            w.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            &backup,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?,
    ))
}

async fn authenticated_activity(
    wallet: &Paykit,
    key: &[u8; 32],
    side: ActivitySide,
) -> TestResult<ActivityRecord> {
    let held = wallet
        .app()
        .shared_record(key)
        .await?
        .ok_or("missing activity")?;
    assert!(
        held.authenticated
            && !held.flagged
            && !held.not_taken
            && !held.doubtful
            && !held.different_write_at_held_version,
        "authenticated={} flagged={} different={} doubtful={} version={:?}",
        held.authenticated,
        held.flagged,
        held.different_write_at_held_version,
        held.doubtful,
        held.version
    );
    Ok(ActivityRecord::read(
        held.content.value().ok_or("unreadable activity")?,
        &wallet.identity(),
        side,
    )?)
}

async fn open(
    wallet: &mut Paykit,
    obligation: &[u8; 32],
    attempt: &paykit_lib::AttemptId,
    owner: [u8; 16],
) -> TestResult {
    let claim = wallet
        .app_mut()
        .read_claim(obligation)
        .await?
        .ok_or("missing claim")?;
    assert_ne!(claim.view.status, ClaimStatus::Settled);
    assert_eq!(claim.view.step, 1);
    let held = claim
        .view
        .attempt(&attempt.to_bytes())
        .ok_or("missing attempt")?;
    assert_eq!((held.owner, held.outcome), (owner, Outcome::Open));
    Ok(())
}

async fn receipt_after_loss(w: &World, remove_writer: bool) -> TestResult {
    let origin = "https://payer.receipt.test";
    let recovery = "https://recovered.receipt.test";
    let rail = Arc::new(FakeRail::with_clock(w.clock().clone()));
    let mut alice = Box::pin(setup_identity(w, "payer", origin)).await?;
    let mut bob = Box::pin(setup_identity(
        w,
        "merchant",
        "https://merchant.receipt.test",
    ))
    .await?;
    let mut payer = Box::pin(payer_wallet(w, &mut alice, "paying-wallet", &rail)).await?;
    let mut phone = Box::pin(payer_wallet(w, &mut alice, "phone", &rail)).await?;
    let mut witness = Box::pin(payer_wallet(w, &mut alice, "record-reader", &rail)).await?;
    let mut shop = Box::new(
        Box::pin(join(
            w,
            &mut bob,
            "merchant",
            "shop",
            AppScopes::new().request().manage_contacts().build()?,
            Arc::new(MemoryStore::new()),
            &rail,
        ))
        .await?,
    );
    payer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&payer.identity()).await?;
    let draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        },
        PaymentReference::new("order")?,
        RequestDestination::Endpoints(BTreeMap::from([(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
            PaymentEndpointPayload::new("lnbc1invoice")?,
        )])),
    );
    let request = Box::pin(shop.send_request(payer.identity(), draft))
        .await?
        .request
        .ok_or("request was not delivered")?;
    Box::pin(payer.receive(None)).await?;
    rail.next(Behaviour::Hang);
    let unpaid = request.clone();
    let (mut payer, payment) = tokio::spawn(Box::pin(async move {
        let paid = Box::pin(payer.pay_request(&unpaid, PayOptions::default())).await;
        (payer, paid.outcome)
    }))
    .await?;
    let payment = payment?;
    let PayOutcome::Pending { attempt_id, .. } = payment else {
        return Err("expected a pending payment".into());
    };
    let owner = payer.app_id();
    let obligation = payer.payments().await?.remove(0).obligation;
    Box::pin(phone.receive(None)).await?;
    while Box::pin(phone.app_mut().list_collections()).await? {}
    open(&mut phone, &obligation, &attempt_id, owner).await?;
    assert!(phone.payments().await?.is_empty());
    let family = paykit_lib::Collection::PayerActivity;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    let key = phone.app().record_key(&target)?;
    rail.settle(&attempt_id, true);
    let paid = tokio::spawn(Box::pin(
        async move { Box::pin(payer.resume_payments()).await },
    ))
    .await??;
    assert!(matches!(paid.as_slice(), [PayOutcome::Paid { .. }]));
    // The reader keeps the original signed record without ever seeing the owner's
    // completed claim. The phone last saw the open attempt and the signed request.
    witness.app_mut().read_record_fresh(&target).await?;
    let proof = authenticated_activity(&witness, &key, ActivitySide::Payer)
        .await?
        .proof
        .ok_or("missing payer proof")?;
    assert!(witness.app().claim_evidence(&obligation).await?.is_none());
    Box::pin(shop.receive(None)).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|s| s.request_hash == Some(request.request_hash))
        .ok_or("missing original sale")?
        .record;
    w.advance_ms(1000);
    if remove_writer {
        let doc = Box::pin(alice.revoke(
            owner,
            None,
            pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
        ))
        .await?;
        assert!(doc.listed_cert(&owner).is_none());
        Box::pin(phone.app_mut().refresh_identity()).await?;
        Box::pin(witness.app_mut().refresh_identity()).await?;
    }
    let proof_backup = Box::pin(witness.backup()).await?;
    w.lose_switch(origin);
    w.add_switch(recovery, &[&w.identity("payer")?]).await?;
    Box::pin(alice.declare_provider_loss(
        recovery,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    w.advance_ms(1000);
    alice.end_staged_admission().await?;
    Box::pin(phone.app_mut().refresh_identity())
        .await
        .map_err(|e| format!("refresh recovered payer: {e}"))?;
    Box::pin(phone.app_mut().follow_provider())
        .await
        .map_err(|e| format!("follow recovered payer: {e}"))?;
    while Box::pin(phone.app_mut().list_collections()).await? {}
    Box::pin(phone.receive(None))
        .await
        .map_err(|e| format!("read recovered payer: {e}"))?;
    phone
        .app_mut()
        .invite(&shop.identity())
        .await
        .map_err(|e| format!("invite merchant: {e}"))?;
    shop.app_mut()
        .identity_of(&phone.identity(), true)
        .await
        .map_err(|e| format!("refresh merchant directory: {e}"))?;
    open(&mut phone, &obligation, &attempt_id, owner).await?;
    let held = authenticated_activity(&phone, &key, ActivitySide::Payer).await?;
    assert_eq!(
        held.request.as_ref().map(StoredMessage::hash),
        Some(request.request_hash)
    );
    assert!(held.proof.is_none() && held.receipt.is_none());
    let earlier = Box::pin(phone.backup()).await?;
    let msg_id = Box::pin(shop.confirm_payment(&sale))
        .await?
        .ok_or("merchant did not issue the receipt")?;
    let issued = authenticated_activity(&shop, &sale, ActivitySide::Payee)
        .await?
        .receipt
        .ok_or("missing merchant receipt")?;
    assert_eq!(issued.message().refs, [proof.hash(), request.request_hash]);
    assert_eq!(issued.message().msg_id, msg_id);
    shop.app_mut().flush_message(&msg_id).await?;
    Box::pin(phone.receive(None)).await?;
    open(&mut phone, &obligation, &attempt_id, owner).await?;
    let kept = phone.app().protocol_scan("paykit", b"pr/").await?;
    assert_eq!(kept.len(), 1);
    assert!(kept[0].0.ends_with(msg_id.as_bytes()));
    let kept_value: serde_json::Value = serde_json::from_slice(&kept[0].1)?;
    assert_eq!(
        kept_value["d"]["plaintext"],
        pubky_switch_core::enc::b64(&issued.plaintext().encode())
    );
    assert_eq!(
        kept_value["d"]["cert"],
        serde_json::to_value(issued.cert().signed())?
    );
    let admitted = Box::pin(phone.backup()).await?;
    drop(phone);
    let older = Box::pin(restore(w, &earlier, &rail)).await?;
    assert!(older
        .app()
        .protocol_scan("paykit", b"pr/")
        .await?
        .is_empty());
    drop(older);
    let mut phone = Box::pin(restore(w, &admitted, &rail)).await?;
    assert_eq!(phone.app().protocol_scan("paykit", b"pr/").await?, kept);
    Box::pin(phone.receive(None)).await?;
    while Box::pin(phone.app_mut().list_collections()).await? {}
    open(&mut phone, &obligation, &attempt_id, owner).await?;
    // A surviving reader can upload the signed write it actually observed. When the
    // final writer has been revoked, try the older reader backup on a fresh installation.
    if remove_writer {
        drop(witness);
        let mut restored = Box::pin(restore(w, &proof_backup, &rail)).await?;
        Box::pin(restored.receive(None)).await?;
    } else {
        Box::pin(witness.app_mut().refresh_identity()).await?;
        Box::pin(witness.app_mut().follow_provider()).await?;
    }
    Box::pin(phone.app_mut().receive(None)).await?;
    phone.app_mut().read_record_fresh(&target).await?;
    let uploaded = phone
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing recovered payer record")?;
    assert!(!uploaded.flagged && !uploaded.not_taken && !uploaded.different_write_at_held_version);
    assert_eq!(uploaded.doubtful, remove_writer);
    if remove_writer {
        // The replacement service cannot date this removed writer's record. Keep the
        // genuine receipt and the unresolved attempt through subsequent reads.
        Box::pin(phone.receive(None)).await?;
        open(&mut phone, &obligation, &attempt_id, owner).await?;
        assert_eq!(phone.app().protocol_scan("paykit", b"pr/").await?, kept);
        assert!(phone
            .receipts(ReceiptsOf::Request(request.clone()))
            .await?
            .is_empty());
        assert_eq!(rail.executions(&attempt_id), 1);
        return Ok(());
    }
    let recovered = ActivityRecord::read(
        uploaded
            .content
            .value()
            .ok_or("unreadable recovered activity")?,
        &phone.identity(),
        ActivitySide::Payer,
    )?
    .proof
    .ok_or("payer proof was not recovered")?;
    assert_eq!(recovered.plaintext().encode(), proof.plaintext().encode());
    let paid_record = phone.app().record_condition(&key).await?;
    open(&mut phone, &obligation, &attempt_id, owner).await?;
    Box::pin(phone.receive(None)).await?;
    Box::pin(phone.receive(None)).await?;
    let placed = authenticated_activity(&phone, &key, ActivitySide::Payer)
        .await?
        .receipt
        .ok_or("receipt was not placed")?;
    assert_eq!(placed.plaintext().encode(), issued.plaintext().encode());
    assert!(phone
        .app()
        .protocol_scan("paykit", b"pr/")
        .await?
        .is_empty());
    let shown = phone.receipts(ReceiptsOf::Request(request.clone())).await?;
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].msg_id, msg_id);
    let evidence = phone.app().shared_evidence().await?;
    let mut completed = Vec::new();
    for held in evidence.ops {
        if held.cert == *phone.app().cert().signed() {
            let op = SignedOp::verify_with_cert(&held.op, phone.app().cert(), &phone.identity())?;
            if let OpKind::Claim(fields) = &op.body().kind {
                let entry = fields.check()?;
                if entry.claim_key == obligation {
                    assert!(op.body().conditions.contains(&paid_record));
                    completed.push(entry);
                }
            }
        }
    }
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].attempt_id, attempt_id.to_bytes());
    assert_eq!(completed[0].expected_step, 1);
    assert_eq!(
        completed[0].kind,
        EntryKind::Done {
            basis: Basis::Evidence,
            evidence_ref: Some(issued.hash())
        }
    );
    let claim = phone
        .app_mut()
        .read_claim(&obligation)
        .await?
        .ok_or("missing completed claim")?;
    assert_eq!(claim.view.status, ClaimStatus::Settled);
    assert_eq!(claim.view.step, 2);
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    assert_eq!(rail.executions(&attempt_id), 1);
    Ok(())
}
