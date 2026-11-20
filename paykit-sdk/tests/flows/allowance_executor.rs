//! The approving app's first grant from its retained original acceptance.

mod enrolled;

use paykit_lib::allowances::{AllowanceTerms, Ledger};
use paykit_lib::records::AllowanceRecord;
use paykit_sdk::{AllowanceRole, AllowanceStatus, AppScopes};
use pubky_switch_client::app::{App, UserConfirmed};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::ops::{Condition, OpKind, RecordVersion, SignedOp};
use pubky_switch_core::shared::ApplicationRef;
use serde_json::json;

use crate::common::World;

async fn applied_put(
    app: &App,
    key: [u8; 32],
    version: RecordVersion,
) -> (SignedOp, ApplicationRef) {
    let identity = app.identity();
    let service = app
        .own_identity()
        .service_key_of(version.generation)
        .unwrap();
    app.shared_evidence()
        .await
        .unwrap()
        .ops
        .into_iter()
        .find_map(|kept| {
            let cert = paykit_lib::scopes::verify_certificate(&kept.cert, &identity).ok()?;
            let op = SignedOp::verify_with_cert(&kept.op, &cert, &identity).ok()?;
            let OpKind::RecordPut(put) = &op.body().kind else {
                return None;
            };
            let proof = TransitionProof::verify(kept.proof.as_ref()?, &service, &identity).ok()?;
            proof.check_binds(&op).ok()?;
            let revision = proof.record_revision().ok()?;
            (put.key == key
                && revision.generation == version.generation
                && revision.seq == version.version)
                .then_some((op, revision))
        })
        .expect("signed record write and applied service proof")
}

fn terms() -> AllowanceTerms {
    AllowanceTerms::from_json(&json!({
        "asset": "btc", "lifetime_amount_limit": "100", "period_limits": [],
        "per_payment_amount": null, "active_from": null, "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))
    .unwrap()
}

async fn first_grant() -> (World, paykit_sdk::Paykit, paykit_sdk::Paykit, [u8; 32]) {
    let world = World::new();
    let mut payer = Box::pin(world.join(
        &world.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    let terms = terms();
    let proposal =
        Box::pin(payer.propose_executable_allowance(shop.identity(), terms, UserConfirmed::new()))
            .await
            .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    assert_eq!(offer.allowance_id, proposal.allowance_id);
    assert!(Box::pin(payer.resume_allowance_executor(&proposal.record))
        .await
        .is_err());
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let accepted = payer.allowances().await.unwrap().pop().unwrap();
    assert_eq!(accepted.allowance_id, proposal.allowance_id);
    assert_eq!(accepted.status, AllowanceStatus::Accepted);
    assert_eq!(accepted.record, proposal.record);
    assert!(!accepted.consented);
    let key = accepted.record;
    let app = payer.app();
    let permanent = app.shared_record(&key).await.unwrap().unwrap();
    let bound = AllowanceRecord::bind(
        permanent.content.value().unwrap(),
        &payer.identity(),
        app.naming_key(),
        &key,
    )
    .unwrap();
    let acceptance = bound.response.as_ref().unwrap().hash();
    let natural =
        paykit_lib::derive::natural_key::allowance(&shop.identity(), &accepted.allowance_id);
    let ledger_key = paykit_lib::Collection::AllowanceLedgers
        .record_key(app.naming_key(), &natural, None)
        .unwrap();
    let pending_key = paykit_lib::Collection::AllowanceProposals
        .record_key(app.naming_key(), &natural, None)
        .unwrap();
    let zero = app.shared_record(&ledger_key).await.unwrap().unwrap();
    assert_eq!(
        zero.content.value().unwrap(),
        Ledger::initial_value(&shop.identity(), &accepted.allowance_id, &acceptance)
    );
    let (original, permanent_revision) = applied_put(app, key, permanent.version).await;
    let (initial, zero_revision) = applied_put(app, ledger_key, zero.version).await;
    assert_eq!(original.body().batch, initial.body().batch);
    assert_eq!(initial.body().batch.count, 3);
    assert_eq!(permanent_revision.seq + 1, zero_revision.seq);
    use pubky_switch_client::app::{
        Collection as SwitchCollection, KeptReason, RecordRead, RecordTarget,
    };
    let pending = paykit_lib::Collection::AllowanceProposals;
    let pending = RecordTarget::new(
        SwitchCollection::new(pending.name(), pending.fixed_policy()),
        &natural,
    );
    let expected_pending = match payer.app_mut().read_record_fresh(&pending).await.unwrap() {
        RecordRead::Fresh(deleted) => {
            assert_eq!(
                deleted.content,
                pubky_switch_client::app::RecordContent::Deleted
            );
            let (_, revision) = applied_put(payer.app(), pending_key, deleted.version).await;
            assert_eq!(zero_revision.seq + 1, revision.seq);
            Condition::RecordCurrent {
                key: pending_key,
                revision,
                deleted: true,
            }
        }
        RecordRead::Kept {
            reason: KeptReason::NotFound,
            ..
        } => Condition::RecordVersion {
            key: pending_key,
            version: RecordVersion::NONE,
        },
        other => panic!("unexpected pending record after original acceptance: {other:?}"),
    };
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(view.executor_app_id, Some(payer.app_id()));
    assert!(view.consented);
    let shop_record = shop.allowances().await.unwrap().pop().unwrap().record;
    assert!(
        Box::pin(shop.confirm_allowance_executor(&shop_record, UserConfirmed::new()))
            .await
            .is_err()
    );
    let granted = payer
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .version;
    let (write, _) = applied_put(payer.app(), key, granted).await;
    assert!(write.body().conditions.contains(&expected_pending));
    for (key, revision) in [(key, permanent_revision), (ledger_key, zero_revision)] {
        assert!(write.body().conditions.contains(&Condition::RecordCurrent {
            key,
            revision,
            deleted: false
        }));
    }
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    assert_eq!(
        payer
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .version,
        granted
    );
    (world, payer, shop, key)
}

#[tokio::test]
async fn test_first_grant_requires_the_accepted_record_and_repeats_once() {
    Box::pin(first_grant()).await;
}

#[tokio::test]
async fn test_first_grant_confirmation_refuses_a_rewritten_initial_ledger() {
    use pubky_switch_client::app::{Collection, OpBatch, RecordTarget};

    let (_world, mut payer, _shop, key) = Box::pin(first_grant()).await;
    let view = payer.allowance(&key).await.unwrap();
    let natural =
        paykit_lib::derive::natural_key::allowance(&view.counterparty, &view.allowance_id);
    let collection = paykit_lib::Collection::AllowanceLedgers;
    let target = RecordTarget::new(
        Collection::new(collection.name(), collection.fixed_policy()),
        &natural,
    );
    let ledger_key = payer.app().record_key(&target).unwrap();
    let original = payer
        .app()
        .shared_record(&ledger_key)
        .await
        .unwrap()
        .unwrap();
    let grant = payer
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .version;
    let rewrite = payer
        .app_mut()
        .record_op(&target, original.content.value(), Vec::new())
        .await
        .unwrap();
    payer
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(rewrite))
        .await
        .unwrap();
    let rewritten = payer
        .app()
        .shared_record(&ledger_key)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(rewritten.version, original.version);
    assert_eq!(rewritten.content, original.content);
    assert!(Box::pin(payer.resume_allowance_executor(&key))
        .await
        .is_err());
    assert!(
        Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new()))
            .await
            .is_err()
    );
    assert_eq!(
        payer
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .version,
        grant
    );
}

#[tokio::test]
async fn test_first_grant_restores_its_complete_original_after_reconciliation() {
    use crate::common::store;
    use paykit_sdk::{Paykit, PaykitConfig, Rail};
    use std::sync::Arc;

    let (world, mut payer, _shop, key) = Box::pin(first_grant()).await;
    let grant = payer
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .version;
    let backup = Box::pin(payer.backup()).await.unwrap();
    let mut restored = Box::pin(Paykit::restore(
        world.env(&world.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&world.rail) as Arc<dyn Rail>,
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    assert!(Box::pin(restored.resume_allowance_executor(&key))
        .await
        .is_err());
    let mut reconciled = None;
    for _ in 0..8 {
        match Box::pin(restored.reconcile()).await {
            Ok(report) => {
                reconciled = Some(report);
                break;
            }
            Err(paykit_sdk::PaykitError::Refused(paykit_sdk::Refusal::NotNow(reason))) => {
                assert!(reason.contains("Call reconcile again"), "{reason}");
            }
            Err(error) => panic!("{error}"),
        }
    }
    let report = reconciled.expect("the restored wallet completed its listing");
    assert!(!report.payer_skipped);
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(restored.resume_allowance_executor(&key))
        .await
        .unwrap();
    Box::pin(restored.confirm_allowance_executor(&key, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(
        restored
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .version,
        grant
    );
    assert!(restored.allowance(&key).await.unwrap().consented);
}

#[derive(Clone, Copy)]
enum RestoredGrantEvidence {
    Matching,
    DifferentPendingValue,
    UnrelatedVerifiedWrite,
}

async fn restore_grant_proof_order(installed_verified: bool, evidence: RestoredGrantEvidence) {
    use crate::common::store;
    use paykit_sdk::{Paykit, PaykitBackup, PaykitConfig, Rail};
    use std::sync::Arc;

    let world = World::new();
    let installed = store();
    let mut payer = Box::pin(world.join_with(
        &world.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        PaykitConfig::default(),
        installed.clone(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    let proposal = Box::pin(payer.propose_executable_allowance(
        shop.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    shop.accept_allowance(&offer.record).await.unwrap();
    payer.receive(None).await.unwrap();
    let key = proposal.record;
    let ungranted = payer.app().shared_record(&key).await.unwrap().unwrap();
    let other_hash = pubky_switch_core::enc::hash(ungranted.content.value().unwrap());
    let original_write = payer
        .app()
        .shared_evidence()
        .await
        .unwrap()
        .ops
        .into_iter()
        .find(|kept| {
            let signed =
                SignedOp::verify_with_cert(&kept.op, payer.app().cert(), &payer.identity())
                    .unwrap();
            matches!(&signed.body().kind, OpKind::RecordPut(put) if put.key == key)
        })
        .unwrap();
    world
        .alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    assert!(Box::pin(payer.resume_allowance_executor(&key))
        .await
        .is_err());
    payer.receive(None).await.unwrap();
    let at = format!("lv/{}", pubky_switch_core::enc::b64(&key));
    let checkpoint = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).unwrap();
    let awaiting = payer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        checkpoint(&awaiting)["d"][0]["grant"]["kind"],
        "awaiting_proof"
    );
    let mut pending_backup = payer.backup().await.unwrap();
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let verified = payer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint(&verified)["d"][0]["grant"]["kind"], "verified");
    let verified_backup = payer.backup().await.unwrap();
    let grant = payer.app().shared_record(&key).await.unwrap().unwrap();
    let (_, revision) = applied_put(payer.app(), key, grant.version).await;
    let writes = world.record_writes(&world.alice, &key);
    drop(payer);

    if matches!(evidence, RestoredGrantEvidence::DifferentPendingValue) {
        // The backup's pending marker names another current-format Allowance value.
        let backup = serde_json::from_slice(&pending_backup.to_bytes().unwrap()).unwrap();
        let mut layer = crate::restore::layer_of(&backup);
        let (_, row) = layer
            .iter_mut()
            .find(|(k, _)| k.as_slice() == at.as_bytes())
            .unwrap();
        row["d"][0]["grant"]["value_hash"] = pubky_switch_core::enc::b64(&other_hash).into();
        pending_backup = PaykitBackup::from_bytes(
            &serde_json::to_vec(&crate::restore::with_layer(&backup, layer)).unwrap(),
        )
        .unwrap();
    }
    let (storage, mut selected) = if installed_verified {
        (installed, pending_backup)
    } else {
        let storage = store();
        let pending = Box::pin(Paykit::restore(
            world.env(&world.alice, storage.clone()),
            PaykitConfig::default(),
            &pending_backup,
            Arc::clone(&world.rail) as Arc<dyn Rail>,
            world.timer(),
        ))
        .await
        .unwrap();
        assert_eq!(
            checkpoint(
                &pending
                    .app()
                    .protocol_get("paykit", at.as_bytes())
                    .await
                    .unwrap()
                    .unwrap()
            )["d"][0]["grant"]["kind"],
            "awaiting_proof"
        );
        drop(pending);
        (storage, verified_backup)
    };
    if matches!(evidence, RestoredGrantEvidence::UnrelatedVerifiedWrite) {
        // This signed original acceptance is genuine, but proves no executor grant.
        let backup = serde_json::from_slice(&selected.to_bytes().unwrap()).unwrap();
        let mut layer = crate::restore::layer_of(&backup);
        let (_, row) = layer
            .iter_mut()
            .find(|(k, _)| k.as_slice() == at.as_bytes())
            .unwrap();
        let mut unrelated = row["d"][0].clone();
        unrelated["grant"] = json!({ "kind": "verified", "write": original_write });
        row["d"].as_array_mut().unwrap().push(unrelated);
        selected = PaykitBackup::from_bytes(
            &serde_json::to_vec(&crate::restore::with_layer(&backup, layer)).unwrap(),
        )
        .unwrap();
    }
    let mut restored = Box::pin(Paykit::restore(
        world.env(&world.alice, storage),
        PaykitConfig::default(),
        &selected,
        Arc::clone(&world.rail) as Arc<dyn Rail>,
        world.timer(),
    ))
    .await
    .unwrap();
    let history = checkpoint(
        &restored
            .app()
            .protocol_get("paykit", at.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    );
    let expected = if matches!(evidence, RestoredGrantEvidence::UnrelatedVerifiedWrite) {
        3
    } else {
        2
    };
    assert_eq!(history["d"].as_array().unwrap().len(), expected);
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    assert!(Box::pin(restored.resume_allowance_executor(&key))
        .await
        .is_err());
    let mut complete = false;
    for _ in 0..8 {
        match Box::pin(restored.reconcile()).await {
            Ok(report) => {
                assert!(!report.payer_skipped);
                complete = true;
                break;
            }
            Err(paykit_sdk::PaykitError::Refused(paykit_sdk::Refusal::NotNow(reason))) => {
                assert!(reason.contains("Call reconcile again"), "{reason}");
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(complete);
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    if !matches!(evidence, RestoredGrantEvidence::Matching) {
        assert!(Box::pin(restored.resume_allowance_executor(&key))
            .await
            .is_err());
        assert!(
            Box::pin(restored.confirm_allowance_executor(&key, UserConfirmed::new()))
                .await
                .is_err()
        );
    } else {
        Box::pin(restored.resume_allowance_executor(&key))
            .await
            .unwrap();
        Box::pin(restored.confirm_allowance_executor(&key, UserConfirmed::new()))
            .await
            .unwrap();
        assert!(restored.allowance(&key).await.unwrap().consented);
    }
    assert_eq!(world.record_writes(&world.alice, &key), writes);
    let current = restored.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(current.version, grant.version);
    assert_eq!(
        applied_put(restored.app(), key, current.version).await.1,
        revision
    );
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_first_grant_restore_matches_an_older_pending_proof() {
    Box::pin(restore_grant_proof_order(
        true,
        RestoredGrantEvidence::Matching,
    ))
    .await;
}

#[tokio::test]
async fn test_first_grant_restore_matches_a_newer_verified_proof() {
    Box::pin(restore_grant_proof_order(
        false,
        RestoredGrantEvidence::Matching,
    ))
    .await;
}

#[tokio::test]
async fn test_first_grant_restore_holds_a_different_pending_value() {
    for installed_verified in [true, false] {
        Box::pin(restore_grant_proof_order(
            installed_verified,
            RestoredGrantEvidence::DifferentPendingValue,
        ))
        .await;
    }
}

#[tokio::test]
async fn test_first_grant_restore_holds_evidence_without_the_original_grant() {
    for installed_verified in [true, false] {
        Box::pin(restore_grant_proof_order(
            installed_verified,
            RestoredGrantEvidence::UnrelatedVerifiedWrite,
        ))
        .await;
    }
}

#[tokio::test]
async fn test_payer_requires_combined_confirmation_before_proposing_or_accepting() {
    let world = World::new();
    let mut payer = Box::pin(world.join(
        &world.alice,
        "payments",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    let proposal =
        Box::pin(shop.propose_allowance(payer.identity(), AllowanceRole::Allowee, terms()))
            .await
            .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let offer = payer.allowances().await.unwrap().pop().unwrap();
    assert_eq!(offer.allowance_id, proposal.allowance_id);
    assert!(Box::pin(payer.accept_allowance(&offer.record))
        .await
        .is_err());
    assert_eq!(
        payer.allowance(&offer.record).await.unwrap().status,
        AllowanceStatus::Proposed
    );
    assert!(
        Box::pin(payer.propose_allowance(shop.identity(), AllowanceRole::Allower, terms()))
            .await
            .is_err()
    );
    assert!(Box::pin(payer.resume_allowance_executor(&offer.record))
        .await
        .is_err());
    assert!(
        Box::pin(payer.confirm_allowance_executor(&offer.record, UserConfirmed::new()))
            .await
            .is_err()
    );
    Box::pin(shop.receive(None)).await.unwrap();
    assert_eq!(
        shop.allowance(&proposal.record).await.unwrap().status,
        AllowanceStatus::Proposed
    );
    assert_eq!(shop.allowances().await.unwrap().len(), 1);
    assert!(payer
        .allowance(&offer.record)
        .await
        .unwrap()
        .executor_app_id
        .is_none());
}

#[tokio::test]
async fn test_another_wallet_and_an_older_backup_cannot_grant_the_accepted_allowance() {
    Box::pin(resume_seller_proposal(false)).await;
}

#[tokio::test]
async fn test_reopened_payer_incorporates_the_same_unanswered_grant() {
    Box::pin(resume_seller_proposal(true)).await;
}

async fn resume_seller_proposal(lose_answer: bool) {
    use crate::common::store;
    use paykit_sdk::{Paykit, PaykitConfig, Rail};
    use std::sync::Arc;

    let world = World::new();
    let payer_store = store();
    let mut payer = Box::pin(world.join_with(
        &world.alice,
        "payments",
        AppScopes::new().allowance_execute().build().unwrap(),
        PaykitConfig::default(),
        payer_store.clone(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    let old_backup = Box::pin(payer.backup()).await.unwrap();
    let proposed =
        Box::pin(shop.propose_allowance(payer.identity(), AllowanceRole::Allowee, terms()))
            .await
            .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let offer = payer.allowances().await.unwrap().pop().unwrap();
    assert_eq!(offer.allowance_id, proposed.allowance_id);
    Box::pin(payer.accept_executable_allowance(&offer.record, UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let accepted = payer.allowance(&offer.record).await.unwrap();
    assert_eq!(accepted.status, AllowanceStatus::Accepted);
    let key = accepted.record;
    let mut other = Box::pin(world.join(
        &world.alice,
        "other payments",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    assert_ne!(other.app_id(), payer.app_id());
    Box::pin(other.receive(None)).await.unwrap();
    assert_eq!(
        other.allowance(&key).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    assert!(Box::pin(other.resume_allowance_executor(&key))
        .await
        .is_err());

    if !lose_answer {
        let mut older = Box::pin(Paykit::restore(
            world.env(&world.alice, store()),
            PaykitConfig::default(),
            &old_backup,
            Arc::clone(&world.rail) as Arc<dyn Rail>,
            world.timer(),
        ))
        .await
        .unwrap();
        Box::pin(crate::common::finish_listing(&mut older)).await;
        assert_eq!(older.app_id(), payer.app_id());
        assert_eq!(
            older.allowance(&key).await.unwrap().status,
            AllowanceStatus::Accepted
        );
        assert!(Box::pin(older.resume_allowance_executor(&key))
            .await
            .is_err());
    }
    let app_id = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(world.load_on_clock(
        &world.alice,
        payer_store.clone(),
        app_id,
        world.clock.clone(),
    ))
    .await;
    if lose_answer {
        world
            .alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
                path: "/ops".into(),
            });
        assert!(Box::pin(payer.resume_allowance_executor(&key))
            .await
            .is_err());
        Box::pin(other.receive(None)).await.unwrap();
        let grant = other.app().shared_record(&key).await.unwrap().unwrap();
        let (_, committed) = applied_put(other.app(), key, grant.version).await;
        drop(payer);
        payer =
            Box::pin(world.load_on_clock(&world.alice, payer_store, app_id, world.clock.clone()))
                .await;
        Box::pin(payer.receive(None)).await.unwrap();
        Box::pin(payer.resume_allowance_executor(&key))
            .await
            .unwrap();
        let grant = payer.app().shared_record(&key).await.unwrap().unwrap();
        assert_eq!(
            committed,
            applied_put(payer.app(), key, grant.version).await.1
        );
    }
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let granted = payer.allowance(&key).await.unwrap();
    assert_eq!(granted.executor_app_id, Some(app_id));
    assert!(granted.consented);
}
