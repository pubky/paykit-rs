//! The approving app's first grant from its retained original acceptance.

mod enrolled;
mod refusals;
mod regrant;

use paykit_lib::allowances::{AllowanceTerms, EntryOutcome, Ledger};
use paykit_lib::records::AllowanceRecord;
use paykit_sdk::{AllowanceRole, AllowanceStatus, AppScopes};
use pubky_dispatch_client::app::{App, UserConfirmed};
use pubky_dispatch_core::chain::TransitionProof;
use pubky_dispatch_core::ops::{Condition, OpKind, RecordVersion, SignedOp};
use pubky_dispatch_core::shared::ApplicationRef;
use serde_json::json;

use crate::common::{draft, request_from, World, HOUR_MS};
use pubky_dispatch_client::clock::Clock;

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
    Box::pin(first_grant_in(crate::common::store(), terms())).await
}

async fn first_grant_in(
    store: std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>,
    terms: AllowanceTerms,
) -> (World, paykit_sdk::Paykit, paykit_sdk::Paykit, [u8; 32]) {
    Box::pin(first_grant_as(
        store,
        terms,
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await
}

/// The first grant, the payer's wallet joined with `scopes`.
async fn first_grant_as(
    store: std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>,
    terms: AllowanceTerms,
    scopes: paykit_sdk::RequestedScopes,
) -> (World, paykit_sdk::Paykit, paykit_sdk::Paykit, [u8; 32]) {
    let world = World::new();
    let mut payer =
        Box::pin(world.join_with(&world.alice, "wallet", scopes, Default::default(), store)).await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
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
    use pubky_dispatch_client::app::{
        Collection as DispatchCollection, KeptReason, RecordRead, RecordTarget,
    };
    let pending = paykit_lib::Collection::AllowanceProposals;
    let pending = RecordTarget::new(
        DispatchCollection::new(pending.name(), pending.fixed_policy()),
        &natural,
    );
    let expected_pending = match payer.app_mut().read_record_fresh(&pending).await.unwrap() {
        RecordRead::Fresh(deleted) => {
            assert_eq!(
                deleted.content,
                pubky_dispatch_client::app::RecordContent::Deleted
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
    assert!(Box::pin(shop.confirm_allowance_executor(
        &shop.allowance(&shop_record).await.unwrap().hold,
        UserConfirmed::new()
    ))
    .await
    .is_err());
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
async fn test_unanswered_owner_resolution_advances_the_reserved_ledger_once() {
    use paykit_lib::allowances::EntryOutcome;
    use paykit_sdk::Resolution;
    use pubky_dispatch_client::testing::Fault;

    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    let identity = payer.identity();
    let app_id = payer.app_id();
    world.rail.set_history_payer(identity);
    let attempt_id = Box::pin(open_funded_request(
        &world, &mut payer, &mut shop, &key, "7",
    ))
    .await;
    let ledger = payer
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let reservation = Ledger::from_json(ledger.content.value().unwrap()).unwrap();
    let entry = &reservation.entries[0];
    assert_eq!((entry.attempt_id, entry.outcome), (attempt_id, None));
    let writes = world.record_writes(&world.alice, &ledger.key);
    world.rail.settle(&attempt_id, false);
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(Box::pin(payer.resolve_attempt(
        &entry.obligation,
        attempt_id,
        Resolution::Failed,
        UserConfirmed::new()
    ))
    .await
    .is_err());
    assert_eq!(world.record_writes(&world.alice, &ledger.key), writes + 1);
    let backup = Box::pin(payer.backup()).await.unwrap();
    drop(payer);

    let mut reopened =
        Box::pin(world.load_on_clock(&world.alice, store.clone(), app_id, world.clock.clone()))
            .await;
    Box::pin(crate::common::finish_listing(&mut reopened)).await;
    let usage = Box::pin(reopened.reconcile_allowance(&key))
        .await
        .unwrap()
        .expect("applied resolution");
    // The owner's failure releases the 7 (Paykit spec 8.3, `failed` releases).
    assert_eq!(
        (usage.lifetime().as_str(), usage.spent(), usage.released()),
        ("0", 0, 1)
    );
    let applied = reopened
        .app()
        .shared_record(&ledger.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Ledger::from_json(applied.content.value().unwrap())
            .unwrap()
            .entries[0]
            .outcome,
        Some(EntryOutcome::Failed)
    );
    let (_, resolution) = applied_put(reopened.app(), ledger.key, applied.version).await;
    let checkpoint = Box::pin(reopened.backup()).await.unwrap();
    drop(reopened);
    let older_store = crate::common::store();
    let mut restored = Box::pin(restore_funded_wallet(&world, older_store.clone(), &backup)).await;
    assert_eq!(
        Box::pin(restored.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap(),
        usage
    );
    assert_eq!(
        restored
            .app()
            .shared_record(&ledger.key)
            .await
            .unwrap()
            .unwrap()
            .version,
        applied.version
    );
    assert_eq!(world.record_writes(&world.alice, &ledger.key), writes + 1);
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
    drop(restored);

    // A second installation of the same executor continues from that signed resolution.
    let mut second = Box::pin(restore_funded_wallet(
        &world,
        crate::common::store(),
        &checkpoint,
    ))
    .await;
    Box::pin(finish_restored_reconciliation(&mut second)).await;
    Box::pin(second.confirm_allowance_executor(
        &second.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let next_id = Box::pin(open_funded_request(
        &world,
        &mut second,
        &mut shop,
        &key,
        "3",
    ))
    .await;
    let successor = second
        .app()
        .shared_record(&ledger.key)
        .await
        .unwrap()
        .unwrap();
    let (signed, _) = applied_put(second.app(), ledger.key, successor.version).await;
    assert!(signed
        .body()
        .conditions
        .contains(&Condition::RecordCurrent {
            key: ledger.key,
            revision: resolution,
            deleted: false,
        }));
    assert_eq!(world.record_writes(&world.alice, &ledger.key), writes + 2);

    // The installed store still holds the signed resolution. The imported backup
    // has the pending batch, and the service now holds its single direct successor.
    let mut combined = Box::pin(restore_funded_wallet(&world, store, &backup)).await;
    let counted = Box::pin(combined.reconcile_allowance(&key))
        .await
        .unwrap()
        .expect("the installed resolution and the service's direct reservation both verify");
    assert_eq!(
        (
            counted.lifetime().as_str(),
            counted.spent(),
            counted.reserved()
        ),
        ("3", 0, 1)
    );
    assert_eq!(
        combined
            .app()
            .shared_record(&ledger.key)
            .await
            .unwrap()
            .unwrap()
            .version,
        successor.version
    );
    assert_eq!(world.record_writes(&world.alice, &ledger.key), writes + 2);
    assert_eq!(world.rail.pay_calls(&next_id), 1);
    drop(combined);

    // The other installed store accepted the same resolution, but is now missing
    // two moves of the ledger. Knowing the old batch succeeded cannot fill that gap, so
    // the restored executor refuses for good, as one restored behind its ledger.
    let next_ledger = Ledger::from_json(successor.content.value().unwrap()).unwrap();
    let next_entry = next_ledger
        .entries
        .iter()
        .find(|e| e.attempt_id == next_id)
        .unwrap();
    world.rail.settle(&next_id, false);
    Box::pin(crate::common::finish_listing(&mut second)).await;
    Box::pin(second.resolve_attempt(
        &next_entry.obligation,
        next_id,
        Resolution::Failed,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    drop(second);
    let mut gap = Box::pin(restore_funded_wallet(&world, older_store, &backup)).await;
    let behind = Box::pin(gap.reconcile_allowance(&key)).await.unwrap_err();
    assert_eq!(
        behind.refusal(),
        Some(&paykit_sdk::Refusal::HeldForUser(
            paykit_sdk::HeldForUserReason::AllowanceNotResumedAfterRestore
        )),
        "{behind:?}"
    );
    assert_eq!(world.record_writes(&world.alice, &ledger.key), writes + 3);
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
}

async fn restore_funded_wallet(
    world: &World,
    store: std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>,
    backup: &paykit_sdk::PaykitBackup,
) -> paykit_sdk::Paykit {
    let mut wallet = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, store),
        Default::default(),
        backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut wallet)).await;
    wallet
}

async fn open_funded_request(
    world: &World,
    payer: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    key: &[u8; 32],
    amount: &str,
) -> paykit_lib::AttemptId {
    let request = Box::pin(request_from(
        shop,
        payer.identity(),
        draft(amount, world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let pending = Box::pin(payer.pay_under_allowance(&request, key))
        .await
        .outcome
        .unwrap_or_else(|error| panic!("opening the {amount}-unit reservation: {error}"));
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = pending else {
        panic!("{pending:?}");
    };
    attempt_id
}

#[tokio::test]
async fn test_original_grant_and_ledger_require_the_payers_complete_history() {
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    // This hand payment shares the wallet account but spends none of the Allowance.
    Box::pin(hand_payment(&world, &mut payer, &mut shop, "0.4999999991")).await;
    world.rail.set_history_payer(shop.identity());
    assert!(Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_none());
    world.rail.set_history_payer(payer.identity());
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.lifetime().as_str(), "0");
    let before = payer.app().cert().clone();
    let scopes = AppScopes::new().allowance_execute().build().unwrap();
    let renewed = crate::certificate::renew(&world, payer.app_id(), &scopes, 1_000);
    world.clock.advance_ms(1_001);
    crate::certificate::list(&world, payer.app_id(), &before, &renewed);
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(payer.app().cert().hash(), renewed.hash());
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "0"
    );

    let mut second = Box::pin(world.join(
        &world.alice,
        "travel wallet",
        AppScopes::new().pay().build().unwrap(),
    ))
    .await;
    assert_ne!(second.app_id(), payer.app_id());
    second.app_mut().invite(&shop.identity()).await.unwrap();
    Box::pin(hand_payment(&world, &mut second, &mut shop, "3")).await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "0"
    );
    // A complete wallet scan also includes this handoff made outside Paykit. No signed
    // Paykit proof establishes why it was sent, so the reader withholds usage.
    world
        .rail
        .pay_outside(
            crate::common::endpoint("btc-lightning-bolt11"),
            paykit_lib::PaymentEndpointPayload::new("ln-other").unwrap(),
            crate::common::btc("2"),
            paykit_lib::AttemptId::from_bytes([8; 16]),
        )
        .await
        .unwrap();
    assert!(Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_none());
}

async fn hand_payment(
    world: &World,
    payer: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    amount: &str,
) {
    let request = Box::pin(request_from(
        shop,
        world.alice_id(),
        draft(amount, world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let paid = Box::pin(payer.pay_request(&request, paykit_sdk::PayOptions::default()))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, paykit_sdk::PayOutcome::Paid { .. }));
}

#[tokio::test]
async fn test_first_grant_confirmation_refuses_a_rewritten_initial_ledger() {
    use pubky_dispatch_client::app::{Collection, OpBatch, RecordTarget};

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
    assert!(Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new()
    ))
    .await
    .is_err());
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

async fn finish_restored_reconciliation(restored: &mut paykit_sdk::Paykit) {
    for _ in 0..8 {
        match Box::pin(restored.reconcile()).await {
            Ok(report) => {
                assert!(!report.payer_skipped);
                Box::pin(restored.end_reconcile(UserConfirmed::new()))
                    .await
                    .unwrap();
                return;
            }
            Err(paykit_sdk::PaykitError::Refused(paykit_sdk::Refusal::NotNow(reason))) => {
                assert_eq!(
                    reason,
                    paykit_sdk::NotNowReason::RecordsUnlisted {
                        retry: paykit_sdk::ListingRetry::Reconcile
                    },
                    "{reason}"
                );
            }
            Err(error) => panic!("{error}"),
        }
    }
    panic!("the restored wallet did not finish reconciliation");
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
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    Box::pin(restored.resume_allowance_executor(&key))
        .await
        .unwrap();
    Box::pin(restored.confirm_allowance_executor(
        &restored.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
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
    LaterReservations,
    DifferentPendingValue,
    UnrelatedVerifiedWrite,
}

// Each reservation opens its own claim in the same batch, before any method handoff.
async fn reserve_before_restore(
    world: &World,
    payer: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    key: &[u8; 32],
    reservations: &[(&str, &str, u64)],
) {
    use paykit_lib::allowances::LedgerEntry;
    use pubky_dispatch_client::app::{
        ClaimOp, Collection, ObligationDeadline, OpBatch, RecordTarget,
    };

    let view = payer.allowance(key).await.unwrap();
    let natural =
        paykit_lib::derive::natural_key::allowance(&view.counterparty, &view.allowance_id);
    let collection = paykit_lib::Collection::AllowanceLedgers;
    let target = RecordTarget::new(
        Collection::new(collection.name(), collection.fixed_policy()),
        &natural,
    );
    let ledger_key = payer.app().record_key(&target).unwrap();
    let grant = payer.app().shared_record(key).await.unwrap().unwrap();
    let (_, revision) = applied_put(payer.app(), *key, grant.version).await;
    let grant = Condition::RecordCurrent {
        key: *key,
        revision,
        deleted: false,
    };
    world.rail.set_history_payer(payer.identity());
    for &(amount, total, count) in reservations {
        let at = world.clock.now_ms();
        let request = Box::pin(request_from(shop, payer.identity(), draft(amount, at))).await;
        Box::pin(payer.receive(None)).await.unwrap();
        let obligation = paykit_lib::derive::request_obligation(
            payer.app().naming_key(),
            &request.payee,
            &request.request_hash,
            0,
        );
        let attempt = paykit_lib::AttemptId::generate().unwrap();
        let claim = ClaimOp::attempt(obligation, 0, attempt.to_bytes(), 4200, None);
        let current = payer
            .app()
            .shared_record(&ledger_key)
            .await
            .unwrap()
            .unwrap();
        let (_, revision) = applied_put(payer.app(), ledger_key, current.version).await;
        let mut ledger = Ledger::from_json(current.content.value().unwrap()).unwrap();
        let amount = crate::common::btc(amount);
        ledger.entries.push(LedgerEntry {
            obligation,
            attempt_id: attempt,
            value: amount.value,
            asset: amount.asset,
            at,
            outcome: None,
        });
        let conditions = vec![
            grant,
            Condition::RecordCurrent {
                key: ledger_key,
                revision,
                deleted: false,
            },
            Condition::ServerTimeIn {
                start: Some(at),
                end: Some(terms().entry_window_end(at).unwrap()),
            },
        ];
        let write = payer
            .app_mut()
            .record_op(&target, Some(&ledger.to_json().unwrap()), conditions)
            .await
            .unwrap();
        let batch = OpBatch::for_protocol("paykit")
            .op_if(claim.kind(), vec![grant])
            .obligation_deadline(obligation, ObligationDeadline::NoDeadline)
            .entry(write);
        Box::pin(payer.app_mut().submit_batch(batch)).await.unwrap();
        Box::pin(assert_reserved_usage(world, payer, key, total, count)).await;
    }
}

async fn assert_reserved_usage(
    world: &World,
    payer: &mut paykit_sdk::Paykit,
    key: &[u8; 32],
    total: &str,
    count: u64,
) {
    let before = world.router.log().len();
    let usage = Box::pin(payer.reconcile_allowance(key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.lifetime().as_str(), total);
    assert_eq!(
        (usage.reserved(), usage.spent(), usage.released()),
        (count, 0, 0)
    );
    let requests = world.router.log();
    let claims: Vec<_> = requests[before..]
        .iter()
        .filter(|r| r.starts_with("GET ") && r.contains("/claims/"))
        .collect();
    assert_eq!(claims.len() as u64, count);
    assert_eq!(
        claims
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        claims.len()
    );
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
    let other_hash = pubky_dispatch_core::enc::hash(ungranted.content.value().unwrap());
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
        .fail_next(pubky_dispatch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    assert!(Box::pin(payer.resume_allowance_executor(&key))
        .await
        .is_err());
    payer.receive(None).await.unwrap();
    let at = format!("lv/{}", pubky_dispatch_core::enc::b64(&key));
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
    if matches!(evidence, RestoredGrantEvidence::LaterReservations) {
        Box::pin(reserve_before_restore(
            &world,
            &mut payer,
            &mut shop,
            &key,
            &[("40", "40", 1), ("30", "70", 2)],
        ))
        .await;
    }
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
        row["d"][0]["grant"]["value_hash"] = pubky_dispatch_core::enc::b64(&other_hash).into();
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
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    if matches!(
        evidence,
        RestoredGrantEvidence::DifferentPendingValue
            | RestoredGrantEvidence::UnrelatedVerifiedWrite
    ) {
        assert!(Box::pin(restored.resume_allowance_executor(&key))
            .await
            .is_err());
        assert!(Box::pin(restored.confirm_allowance_executor(
            &restored.allowance(&key).await.unwrap().hold,
            UserConfirmed::new()
        ))
        .await
        .is_err());
    } else {
        if matches!(evidence, RestoredGrantEvidence::LaterReservations) {
            // The backup predates both received requests. Confirm the existing Allowance
            // after restoring it; the confirmed budget still includes both reservations.
            let view = restored.allowance(&key).await.unwrap();
            assert!(!view.consented);
            assert!(Box::pin(restored.resume_allowance_executor(&key))
                .await
                .is_err());
        } else {
            Box::pin(restored.resume_allowance_executor(&key))
                .await
                .unwrap();
        }
        Box::pin(restored.confirm_allowance_executor(
            &restored.allowance(&key).await.unwrap().hold,
            UserConfirmed::new(),
        ))
        .await
        .unwrap();
        assert!(restored.allowance(&key).await.unwrap().consented);
        if matches!(evidence, RestoredGrantEvidence::LaterReservations) {
            world.rail.set_history_payer(shop.identity());
            assert!(Box::pin(restored.reconcile_allowance(&key))
                .await
                .unwrap()
                .is_none());
            world.rail.set_history_payer(restored.identity());
            Box::pin(assert_reserved_usage(&world, &mut restored, &key, "70", 2)).await;
        }
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
async fn test_restore_holds_a_gap_between_two_noninitial_allowance_ledgers() {
    let storage = crate::common::store();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(storage.clone(), terms())).await;
    Box::pin(reserve_before_restore(
        &world,
        &mut payer,
        &mut shop,
        &key,
        &[("10", "10", 1)],
    ))
    .await;
    let backup = Box::pin(payer.backup()).await.unwrap();
    Box::pin(reserve_before_restore(
        &world,
        &mut payer,
        &mut shop,
        &key,
        &[("20", "30", 2), ("30", "60", 3)],
    ))
    .await;
    drop(payer);
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, storage),
        Default::default(),
        &backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    assert!(Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_none());
    assert!(Box::pin(restored.confirm_allowance_executor(
        &restored.allowance(&key).await.unwrap().hold,
        UserConfirmed::new()
    ))
    .await
    .is_err());
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_of_an_older_pending_grant_keeps_later_reservations() {
    Box::pin(restore_grant_proof_order(
        true,
        RestoredGrantEvidence::LaterReservations,
    ))
    .await;
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

/// After the Allowance at `record` was withdrawn: the executor's resume and a payment under
/// it are both refused, and nothing is written. Returns the two refusals.
pub(super) async fn attempt_after_withdrawal(
    world: &World,
    payer: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    record: &[u8; 32],
) -> (paykit_sdk::PaykitError, paykit_sdk::PaykitError) {
    let writes = world.record_writes(&world.alice, record);
    let grant = Box::pin(payer.resume_allowance_executor(record))
        .await
        .unwrap_err();

    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        shop,
        payer.identity(),
        draft("0.001", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let refused = Box::pin(payer.pay_under_allowance(&request, record))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(world.record_writes(&world.alice, record), writes);
    (grant, refused)
}

#[tokio::test]
async fn test_executor_pays_the_period_of_its_accepted_subscription() {
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
    use paykit_sdk::{PeriodPrice, ReceiptsOf, Refusal};

    let storage = crate::common::store();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(storage.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let start = world.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let periods = schedule.periods(2).unwrap();
    let period = PeriodRef::from(periods[0]);
    let next = PeriodRef::from(periods[1]);
    let req = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("70", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let refused = Box::pin(payer.pay_under_allowance(&req, &key))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Recurring));
    Box::pin(payer.accept_subscription_under_allowance(&req, &key))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    assert!(world.rail.all_executions().is_empty());
    let refused =
        Box::pin(payer.pay_period_automatically(&req, &period, PeriodPrice::RequestedAmount))
            .await
            .outcome
            .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::ManualOnly));
    let backup = Box::pin(payer.backup()).await.unwrap();
    let outcome = Box::pin(payer.pay_period_under_allowance(&req, &period))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let attempt = outcome.attempt_id();
    Box::pin(shop.receive(None)).await.unwrap();
    let activity = shop.payee_activity().await.unwrap();
    let sale = activity
        .iter()
        .find(|a| a.request_hash == Some(req.request_hash))
        .unwrap();
    let occurrence = sale
        .periods
        .iter()
        .find(|p| p.billing_period == period)
        .unwrap();
    let receipt = Box::pin(shop.confirm_payment(&occurrence.record))
        .await
        .unwrap()
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let held = payer
        .receipts(ReceiptsOf::Period(req, period))
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].msg_id, receipt);
    assert_eq!(held[0].attempt_id, attempt);
    assert_eq!(world.rail.executions(&attempt), 1);
    world.clock.set_ms(next.starts_at + 1000);
    let excess = Box::pin(payer.pay_period_under_allowance(&req, &next))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(excess.refusal(), Some(Refusal::NotAutomatic(reason)) if reason.contains("lifetime")),
        "{excess:?}"
    );
    assert_eq!(world.rail.all_executions().len(), 1);
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "70"
    );

    drop(payer);
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, storage),
        Default::default(),
        &backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    let usage = Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .expect("the settled payment of this occurrence stays charged after restore");
    assert_eq!(
        (usage.reserved(), usage.spent(), usage.lifetime().as_str()),
        (0, 1, "70")
    );
    assert_eq!(
        restored
            .receipts(ReceiptsOf::Period(req, period))
            .await
            .unwrap(),
        held
    );
    assert!(restored
        .receipts(ReceiptsOf::Period(req, next))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(world.rail.pay_calls(&attempt), 1);
}

#[tokio::test]
async fn test_executor_refuses_a_request_outside_its_allowance_as_not_automatic() {
    // A one-time request priced by conversion doesn't qualify under an Allowance: final for
    // automatic payment, which no wait changes; the user may still pay it by hand (Paykit
    // spec 8.3).
    use paykit_lib::bodies::Conversion;
    use paykit_lib::{Asset, Decimal, Rate};

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let rate = Rate::new(Asset::new("btc").unwrap(), Decimal::new("2").unwrap()).unwrap();
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()).with_conversion(Conversion::Fixed(vec![rate])),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let refused = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::NotAutomatic(why)) if why.contains("does not qualify")
        ),
        "{refused:?}"
    );
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_funded_subscription_checks_the_deny_list_and_refuses_a_rate_priced_period() {
    use paykit_lib::bodies::Conversion;
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
    use paykit_lib::{Asset, Decimal, Rate};
    use paykit_sdk::Refusal;

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let start = world.clock.now_ms() / 1000 * 1000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let period = PeriodRef::from(schedule.first().unwrap());
    let rate = Rate::new(Asset::new("btc").unwrap(), Decimal::new("2").unwrap()).unwrap();
    let draft = draft("10", start)
        .with_conversion(Conversion::Fixed(vec![rate]))
        .with_recurrence(schedule);
    let request = Box::pin(request_from(&mut shop, payer.identity(), draft)).await;
    Box::pin(payer.receive(None)).await.unwrap();
    let mut contacts = Box::pin(world.join(
        &world.alice,
        "contacts",
        AppScopes::new().pay().manage_contacts().build().unwrap(),
    ))
    .await;
    Box::pin(contacts.block(&shop.identity())).await.unwrap();
    let blocked = Box::pin(payer.accept_subscription_under_allowance(&request, &key))
        .await
        .unwrap_err();
    assert!(
        matches!(blocked.refusal(), Some(Refusal::Blocked { .. })),
        "{blocked:?}"
    );
    Box::pin(contacts.unblock(&shop.identity())).await.unwrap();
    Box::pin(payer.accept_subscription_under_allowance(&request, &key))
        .await
        .unwrap();
    let priced = Box::pin(payer.pay_period_under_allowance(&request, &period))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(priced.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{priced:?}"
    );
    assert!(world.rail.all_executions().is_empty());
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "0"
    );
}

#[tokio::test]
async fn test_funded_period_requiring_its_own_destination_leaves_the_subscription_endpoint_unused()
{
    Box::pin(funded_period_with_older_payable_destination(false)).await;
}

#[tokio::test]
async fn test_funded_period_skips_a_newer_permitted_endpoint_that_requires_a_rate() {
    Box::pin(funded_period_with_older_payable_destination(true)).await;
}

async fn funded_period_with_older_payable_destination(priced: bool) {
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
    use paykit_sdk::Refusal;

    let mut terms = terms();
    let endpoint = crate::common::endpoint("btc-lightning-bolt11");
    let newer = crate::common::endpoint(if priced {
        "eur-sepa-iban"
    } else {
        "btc-lightning-bolt12"
    });
    let identifiers = if priced {
        vec![endpoint.clone(), newer.clone()]
    } else {
        vec![endpoint.clone()]
    };
    terms.allowed_payment_endpoint_identifiers = Some(identifiers.clone());
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(crate::common::store(), terms)).await;
    world.rail.set_history_payer(payer.identity());
    let start = world.clock.now_ms() / 1000 * 1000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let next = PeriodRef::from(schedule.periods(2).unwrap()[1]);
    let mut request = draft("10", start)
        .with_recurrence(schedule)
        .with_per_period_endpoints();
    if priced {
        world.rail.set_endpoint_identifiers(identifiers.clone());
        world.rail.set_fraction_digits(newer.clone(), Some(2));
        request = request
            .with_conversion(paykit_lib::bodies::Conversion::PerPeriod)
            .with_accepted_endpoint_identifiers(identifiers);
        request.destination = paykit_lib::bodies::RequestDestination::RequiredApp(shop.app_id());
        Box::pin(shop.set_list_endpoints(std::collections::BTreeMap::from([(
            endpoint.clone(),
            paykit_lib::PaymentEndpointPayload::new("lnbc1first").unwrap(),
        )])))
        .await
        .unwrap();
        Box::pin(shop.publish_list_to(payer.identity()))
            .await
            .unwrap();
    }
    let req = Box::pin(request_from(&mut shop, payer.identity(), request)).await;
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.accept_subscription_under_allowance(&req, &key))
        .await
        .unwrap();
    world.clock.set_ms(next.starts_at + 1000);
    let result = Box::pin(payer.pay_period_under_allowance(&req, &next))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(result.refusal(), Some(Refusal::NoDestination(_))),
        "{result:?}"
    );
    assert!(world.rail.all_executions().is_empty());
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "0"
    );
    Box::pin(shop.receive(None)).await.unwrap();
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|sale| sale.request_hash == Some(req.request_hash))
        .unwrap();
    let signed = Box::pin(shop.send_period_details(
        &sale.record,
        paykit_lib::bodies::PeriodDetails {
            billing_period: next,
            payment_endpoints: Some(std::collections::BTreeMap::from([(
                endpoint,
                paykit_lib::PaymentEndpointPayload::new("lnbc1next").unwrap(),
            )])),
            rates: None,
            expires_at: next.ends_at,
        },
    ))
    .await
    .unwrap();
    world.clock.advance_ms(1_000);
    Box::pin(
        shop.send_period_details(
            &sale.record,
            paykit_lib::bodies::PeriodDetails {
                billing_period: next,
                payment_endpoints: Some(std::collections::BTreeMap::from([(
                    newer,
                    paykit_lib::PaymentEndpointPayload::new(if priced {
                        "DE00next"
                    } else {
                        "lno1next"
                    })
                    .unwrap(),
                )])),
                rates: priced.then(|| {
                    vec![paykit_lib::Rate::new(
                        paykit_lib::Asset::new("eur").unwrap(),
                        paykit_lib::Decimal::new("2").unwrap(),
                    )
                    .unwrap()]
                }),
                expires_at: next.ends_at,
            },
        ),
    )
    .await
    .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let paid = Box::pin(payer.pay_period_under_allowance(&req, &next))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    assert_eq!(world.rail.pay_calls(&paid.attempt_id()), 1);
    Box::pin(shop.receive(None)).await.unwrap();
    let period = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|sale| sale.request_hash == Some(req.request_hash))
        .unwrap()
        .periods
        .into_iter()
        .find(|entry| entry.billing_period == next)
        .unwrap();
    assert!(period.paid);
    let receipt = Box::pin(shop.confirm_payment(&period.record))
        .await
        .unwrap()
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let received = payer
        .receipts(paykit_sdk::ReceiptsOf::Period(req, next))
        .await
        .unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].msg_id, receipt);
    let held = shop
        .app()
        .shared_record(&period.record)
        .await
        .unwrap()
        .unwrap();
    let credited = paykit_lib::records::ActivityRecord::read(
        held.content.value().unwrap(),
        &shop.identity(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    let selected = credited.period_details.unwrap();
    assert_eq!(selected.message().msg_id, signed.msg_id);
    assert_eq!(
        credited.proof.unwrap().message().refs,
        [req.request_hash, selected.hash()]
    );
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "10"
    );
}

#[tokio::test]
async fn test_executor_incorporates_an_unfunded_period_paid_at_an_authorized_rate() {
    Box::pin(reconcile_receipted_period(PeriodPayment::FirstFixed)).await;
}

#[tokio::test]
async fn test_executor_recovers_a_receipted_fixed_rate_period_with_its_signed_destination() {
    Box::pin(reconcile_receipted_period(PeriodPayment::LaterFixed)).await;
}

#[tokio::test]
async fn test_pruned_same_asset_period_holds_without_its_signed_destination() {
    Box::pin(reconcile_receipted_period(PeriodPayment::LaterRequested)).await;
}

#[derive(Clone, Copy)]
enum PeriodPayment {
    FirstFixed,
    LaterFixed,
    LaterRequested,
}

async fn reconcile_receipted_period(example: PeriodPayment) {
    use paykit_lib::bodies::Conversion;
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
    use paykit_lib::{Asset, Decimal, Rate};
    use paykit_sdk::{PeriodPrice, ReceiptsOf, Refusal};

    let later = !matches!(example, PeriodPayment::FirstFixed);
    let same_asset = matches!(example, PeriodPayment::LaterRequested);
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let start = world.clock.now_ms() / 1000 * 1000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let period = PeriodRef::from(schedule.periods(2).unwrap()[usize::from(later)]);
    let mut subscription = draft("10", start)
        .with_recurrence(schedule)
        .with_period_deadline_s(3600);
    if !same_asset {
        let rate = Rate::new(Asset::new("btc").unwrap(), Decimal::new("2").unwrap()).unwrap();
        subscription = subscription.with_conversion(Conversion::Fixed(vec![rate]));
    }
    let mut subscriptions =
        Box::pin(world.labelled_shop("subscription-wallet", "subscriptions")).await;
    let mut shipping = Box::pin(world.labelled_shop("shipping-wallet", "shipping")).await;
    if later {
        subscription.destination =
            paykit_lib::bodies::RequestDestination::RequiredApp(subscriptions.app_id());
        subscription.per_period_endpoints = true;
        subscription.accepted_endpoint_identifiers =
            Some(vec![crate::common::endpoint("btc-lightning-bolt11")]);
    }
    let req = Box::pin(request_from(
        &mut subscriptions,
        payer.identity(),
        subscription,
    ))
    .await;
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    Box::pin(payer.accept_subscription(&req)).await.unwrap();
    let selected = if later {
        Box::pin(crate::common::finish_listing(&mut subscriptions)).await;
        let sale = subscriptions
            .payee_activity()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.request_hash == Some(req.request_hash))
            .unwrap();
        world.clock.set_ms(period.starts_at + 1000);
        let sent = Box::pin(subscriptions.send_period_details(
            &sale.record,
            paykit_lib::bodies::PeriodDetails {
                billing_period: period,
                payment_endpoints: Some(std::collections::BTreeMap::from([(
                    crate::common::endpoint("btc-lightning-bolt11"),
                    paykit_lib::PaymentEndpointPayload::new("lnbc1later").unwrap(),
                )])),
                rates: None,
                expires_at: period.ends_at,
            },
        ))
        .await
        .unwrap();
        Box::pin(crate::common::finish_listing(&mut payer)).await;
        Some(sent.msg_id)
    } else {
        None
    };
    let shown;
    let price = if same_asset {
        PeriodPrice::RequestedAmount
    } else {
        shown =
            Box::pin(payer.pay_period_automatically(&req, &period, PeriodPrice::RequestedAmount))
                .await
                .outcome
                .unwrap_err();
        let Some(Refusal::PricedByRate { payment, .. }) = shown.refusal() else {
            panic!("{shown:?}")
        };
        PeriodPrice::AtRate(payment)
    };
    let outcome = Box::pin(payer.pay_period_automatically(&req, &period, price))
        .await
        .outcome
        .unwrap();
    let attempt = outcome.attempt_id();
    assert_eq!(
        world.rail.paid_amount(&attempt).unwrap().value.as_str(),
        if same_asset { "10" } else { "20" }
    );
    Box::pin(subscriptions.receive(None)).await.unwrap();
    let sale = subscriptions
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.request_hash == Some(req.request_hash))
        .unwrap();
    let occurrence = sale
        .periods
        .iter()
        .find(|p| p.billing_period == period)
        .unwrap();
    if let Some(id) = selected {
        let held = subscriptions
            .app()
            .shared_record(&occurrence.record)
            .await
            .unwrap()
            .unwrap();
        let record = paykit_lib::records::ActivityRecord::read(
            held.content.value().unwrap(),
            &subscriptions.identity(),
            paykit_lib::records::ActivitySide::Payee,
        )
        .unwrap();
        let details = record.period_details.unwrap();
        assert_eq!(details.message().msg_id, id);
        assert_eq!(
            record.proof.unwrap().message().refs,
            [req.request_hash, details.hash()]
        );
    }
    // This wallet also has a genuine receipt from another of the seller's wallets with a labelled scope.
    let parcel = Box::pin(request_from(
        &mut shipping,
        payer.identity(),
        draft("1", start),
    ))
    .await;
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let parcel_attempt = Box::pin(payer.pay_request(&parcel, Default::default()))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    Box::pin(crate::common::finish_listing(&mut shipping)).await;
    let parcel_sale = shipping
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|sale| sale.attempt_id == Some(parcel_attempt))
        .unwrap();
    let shipping_receipt = Box::pin(shipping.confirm_payment(&parcel_sale.record))
        .await
        .unwrap()
        .unwrap();
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "0"
    );
    // The original payment journal expires 30 days after the deadline.
    world.clock.advance_ms(32 * 86400000);
    let start = world.clock.now_ms() / 1000 * 1000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let next = PeriodRef::from(schedule.first().unwrap());
    let funded = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("10", start).with_recurrence(schedule),
    ))
    .await;
    let next_payment = Box::pin(request_from(&mut shop, payer.identity(), draft("1", start))).await;
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    Box::pin(payer.resume_payments()).await.unwrap();
    assert!(payer
        .payments()
        .await
        .unwrap()
        .iter()
        .all(|entry| entry.attempt_id != attempt));
    let retained = payer.receipts(ReceiptsOf::All).await.unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].msg_id, shipping_receipt);
    assert!(payer
        .receipts(ReceiptsOf::Period(req, period))
        .await
        .unwrap()
        .is_empty());
    if same_asset {
        assert!(Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .is_some());
    } else {
        assert!(Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .is_none());
        let calls = world.rail.all_executions();
        assert!(
            Box::pin(payer.accept_subscription_under_allowance(&funded, &key))
                .await
                .is_err()
        );
        let refused = Box::pin(payer.pay_under_allowance(&next_payment, &key))
            .await
            .outcome
            .unwrap_err();
        assert!(refused.to_string().contains("wallet history"), "{refused}");
        assert_eq!(world.rail.all_executions(), calls);
    }
    let receipt = Box::pin(subscriptions.confirm_payment(&occurrence.record))
        .await
        .unwrap()
        .unwrap();
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    assert_eq!(
        payer
            .receipts(ReceiptsOf::Period(req, period))
            .await
            .unwrap()[0]
            .msg_id,
        receipt
    );
    if same_asset {
        // The proof, receipt and exact method amount survive. A partial activity copy
        // lacks the separate signed destination selected by that proof.
        use paykit_lib::records::{ActivityRecord, ActivitySide};
        let target = super::periods::payer_period_at(&req, period);
        let held = payer.app_mut().read_record(&target).await.unwrap().unwrap();
        let original = held.content.value().unwrap();
        let mut partial =
            ActivityRecord::read(original, &payer.identity(), ActivitySide::Payer).unwrap();
        let selected = partial.period_details.take().unwrap();
        assert_eq!(
            partial.proof.as_ref().unwrap().message().refs,
            [req.request_hash, selected.hash()]
        );
        assert_eq!(partial.receipt.as_ref().unwrap().message().msg_id, receipt);
        let calls = world.rail.all_executions();
        payer
            .app_mut()
            .put_record(&target, &partial.to_json().unwrap(), Vec::new())
            .await
            .unwrap();
        assert!(Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .is_none());
        assert!(Box::pin(payer.pay_under_allowance(&next_payment, &key))
            .await
            .outcome
            .is_err());
        assert_eq!(world.rail.all_executions(), calls);
        payer
            .app_mut()
            .put_record(&target, original, Vec::new())
            .await
            .unwrap();
        assert!(Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .is_some());
        assert_eq!(world.rail.all_executions(), calls);
    }
    Box::pin(payer.accept_subscription_under_allowance(&funded, &key))
        .await
        .unwrap();
    assert!(matches!(
        Box::pin(payer.pay_period_under_allowance(&funded, &next))
            .await
            .outcome
            .unwrap(),
        paykit_sdk::PayOutcome::Paid { .. }
    ));
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "10"
    );
}

#[tokio::test]
async fn test_allowance_payment_enforces_the_remaining_lifetime_amount() {
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    for (amount, allowed) in [("70", true), ("31", false), ("30", true)] {
        let request = Box::pin(request_from(
            &mut shop,
            world.alice_id(),
            draft(amount, world.clock.now_ms()),
        ))
        .await;
        Box::pin(payer.receive(None)).await.unwrap();
        let before = world.rail.all_executions();
        if amount == "70" || amount == "30" {
            world.rail.set_history_payer(shop.identity());
            let held = Box::pin(payer.pay_under_allowance(&request, &key))
                .await
                .outcome;
            assert!(held.unwrap_err().to_string().contains("wallet history"));
            assert_eq!(world.rail.all_executions(), before);
            world.rail.set_history_payer(payer.identity());
        }
        let outcome = Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome;
        if allowed {
            let paykit_sdk::PayOutcome::Paid { attempt_id, .. } = outcome.unwrap() else {
                panic!("payment was not completed")
            };
            assert_eq!(world.rail.executions(&attempt_id), 1);
            if amount == "70" {
                world.rail.set_history_payer(shop.identity());
                assert!(Box::pin(payer.resume_allowance_executor(&key))
                    .await
                    .is_err());
                world.rail.set_history_payer(payer.identity());
                Box::pin(payer.resume_allowance_executor(&key))
                    .await
                    .unwrap();
                Box::pin(shop.receive(None)).await.unwrap();
                let sale = shop
                    .payee_activity()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|v| v.attempt_id == Some(attempt_id))
                    .unwrap();
                assert_eq!(sale.status, paykit_sdk::PayeeStatus::ProofReceived);
                let receipt = Box::pin(shop.confirm_payment(&sale.record))
                    .await
                    .unwrap()
                    .unwrap();
                Box::pin(payer.receive(None)).await.unwrap();
                let held = payer
                    .receipts(paykit_sdk::ReceiptsOf::Request(request))
                    .await
                    .unwrap();
                assert_eq!(held.len(), 1);
                assert_eq!(held[0].msg_id, receipt);
                assert_eq!(held[0].attempt_id, attempt_id);
                assert_eq!(held[0].payee, shop.identity());
                assert_eq!(
                    Box::pin(shop.confirm_payment(&sale.record)).await.unwrap(),
                    Some(receipt)
                );
            }
        } else {
            let error = outcome.unwrap_err();
            assert!(error.to_string().contains("lifetime"), "{error}");
            assert_eq!(world.rail.all_executions(), before);
        }
    }
    let counted = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counted.lifetime().as_str(), "100");
    assert_eq!(world.rail.all_executions().len(), 2);
}

async fn only_ledger(
    payer: &paykit_sdk::Paykit,
) -> (Ledger, pubky_dispatch_core::ops::RecordVersion) {
    let records = payer.app().records_of("allowance_ledgers").await.unwrap();
    assert_eq!(records.len(), 1);
    let held = &records[0];
    (
        Ledger::from_json(held.content.value().unwrap()).unwrap(),
        held.version,
    )
}

#[tokio::test]
async fn test_a_method_proof_in_the_journal_keeps_the_payment_counted() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::testing::Fault;

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = failed else {
        panic!("{failed:?}");
    };
    // The account's history keeps showing the failure; the payment completes after all,
    // and its outcome can't be written: the method's proof is in the journal alone.
    assert!(world.rail.hold_history_outcome(&attempt_id));
    world.rail.complete_after_failure(&attempt_id);
    world.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    // The payer record can't be read either, so its write is held.
    let records = payer.app().records_of("payer_activity").await.unwrap();
    for _ in 0..4 {
        world.alice.switch().fail_next(Fault::Unreachable {
            path: format!(
                "/records/{}",
                pubky_dispatch_core::enc::b64(&records[0].key)
            ),
            after: 0,
        });
    }
    let _ = Box::pin(payer.resume_payments()).await;
    let (ledger, _) = only_ledger(&payer).await;
    assert_eq!(ledger.entries[0].outcome, Some(EntryOutcome::Failed));
    // The journal's proof counts the payment spent over the failure the ledger holds.
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((usage.lifetime().as_str(), usage.released()), ("7", 0));
    // Its outcome waits for the held payer record, as before; nothing paid twice.
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_a_confirmed_failure_is_refused_once_paid_history_was_seen() {
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let id = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let (opened, revision) = only_ledger(&payer).await;
    let obligation = opened.entries[0].obligation;
    // The account's history shows the payment made before this app resumes it; the
    // reconciliation sees it.
    world.rail.settle(&id, true);
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.lifetime().as_str(), "7");
    // The user's confirmation that it failed is refused as already paid; nothing is written.
    let refused = Box::pin(payer.resolve_attempt(
        &obligation,
        id,
        paykit_sdk::Resolution::Failed,
        UserConfirmed::new(),
    ))
    .await
    .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&paykit_sdk::Refusal::AlreadyPaid),
        "{refused:?}"
    );
    assert_eq!(only_ledger(&payer).await.1, revision);
    // The resume records it paid, once.
    Box::pin(payer.resume_payments()).await.unwrap();
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((usage.lifetime().as_str(), usage.spent()), ("7", 1));
    assert_eq!(world.rail.executions(&id), 1);
}

#[tokio::test]
async fn test_a_proof_seen_once_keeps_the_payment_counted_after_its_record_is_gone() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::app::{Collection, RecordTarget};
    use pubky_dispatch_client::testing::Fault;

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = failed else {
        panic!("{failed:?}");
    };
    let obligation = only_ledger(&payer).await.0.entries[0].obligation;
    // The account's history keeps showing the failure; the payment completes after all, and
    // its outcome can't be written yet.
    assert!(world.rail.hold_history_outcome(&attempt_id));
    world.rail.complete_after_failure(&attempt_id);
    world.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(
        only_ledger(&payer).await.0.entries[0].outcome,
        Some(EntryOutcome::Failed)
    );
    // This installation sees the payment made.
    let seen = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((seen.lifetime().as_str(), seen.released()), ("7", 0));
    // The payer record goes and the claim expires before `done` is copied.
    let collection = paykit_lib::Collection::PayerActivity;
    let target = RecordTarget::new(
        Collection::new(collection.name(), collection.fixed_policy()),
        &paykit_lib::derive::natural_key::request_record(&shop.identity(), &request.request_hash),
    );
    payer
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    world
        .clock
        .advance_ms(32 * pubky_dispatch_core::records::DAY_MS);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(payer
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    // What it saw stays: the payment is still counted, not released by the failure the
    // ledger holds.
    let after = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((after.lifetime().as_str(), after.released()), ("7", 0));
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_receive_copies_a_later_owner_failure_once() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::app::{ClaimOp, OpBatch};

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let id = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let (opened, _) = only_ledger(&payer).await;
    let entry = &opened.entries[0];
    assert_eq!(entry.attempt_id, id);
    assert!(entry.outcome.is_none());
    let claim = payer
        .app_mut()
        .read_claim(&entry.obligation)
        .await
        .unwrap()
        .unwrap();
    let op = ClaimOp::failed(entry.obligation, claim.view.step, id.to_bytes());
    payer
        .app_mut()
        .submit_batch(OpBatch::new().op(op.kind()))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let (failed, revision) = only_ledger(&payer).await;
    assert_eq!(failed.entries[0].outcome, Some(EntryOutcome::Failed));
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(only_ledger(&payer).await.1, revision);
    assert_eq!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "0"
    );
    assert_eq!(world.rail.executions(&id), 1);
}

#[tokio::test]
async fn test_older_backup_recovers_one_signed_copy_of_two_owner_failures() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::app::{ClaimOp, OpBatch};

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    for amount in ["5", "7"] {
        let request = Box::pin(request_from(
            &mut shop,
            payer.identity(),
            draft(amount, world.clock.now_ms()),
        ))
        .await;
        Box::pin(payer.receive(None)).await.unwrap();
        world.rail.next(paykit_sdk::rail::Behaviour::Hang);
        Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap();
    }
    let (held, _) = only_ledger(&payer).await;
    assert_eq!(held.entries.len(), 2);
    let backup = Box::pin(payer.backup()).await.unwrap();
    let mut outcomes = OpBatch::new();
    for entry in &held.entries {
        assert!(entry.outcome.is_none());
        world.rail.settle(&entry.attempt_id, false);
        let claim = payer
            .app_mut()
            .read_claim(&entry.obligation)
            .await
            .unwrap()
            .unwrap();
        outcomes = outcomes.op(ClaimOp::failed(
            entry.obligation,
            claim.view.step,
            entry.attempt_id.to_bytes(),
        )
        .kind());
    }
    payer.app_mut().submit_batch(outcomes).await.unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let (after, revision) = only_ledger(&payer).await;
    assert!(after
        .entries
        .iter()
        .all(|e| e.outcome == Some(EntryOutcome::Failed)));
    let app_id = payer.app_id();
    let scopes = AppScopes::new().allowance_execute().build().unwrap();
    for _ in 0..2 {
        let earlier = payer.app().cert().clone();
        world.clock.advance_ms(1_000);
        let renewed = crate::certificate::renew(&world, app_id, &scopes, 0);
        crate::certificate::list(&world, app_id, &earlier, &renewed);
        Box::pin(payer.receive(None)).await.unwrap();
        assert_eq!(payer.app().cert().hash(), renewed.hash());
    }
    let current_cert = payer.app().cert().hash();
    drop(payer);
    let store = crate::common::store();
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, store.clone()),
        Default::default(),
        &backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    assert_eq!(restored.app().cert().hash(), current_cert);
    let usage = Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .expect("both owner failures precede the single copied successor");
    assert_eq!(
        (usage.lifetime().as_str(), usage.spent(), usage.released()),
        ("0", 0, 2)
    );
    assert_eq!(only_ledger(&restored).await.1, revision);
    drop(restored);
    let mut reopened =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    Box::pin(reopened.receive(None)).await.unwrap();
    assert_eq!(
        Box::pin(reopened.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap(),
        usage
    );
    assert_eq!(only_ledger(&reopened).await.1, revision);
    for entry in &held.entries {
        assert_eq!(world.rail.pay_calls(&entry.attempt_id), 1);
    }
}

#[tokio::test]
async fn test_imported_pending_outcome_copy_accepts_a_separately_verified_next_copy() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::testing::Fault;

    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let first = Box::pin(open_funded_request(
        &world, &mut payer, &mut shop, &key, "5",
    ))
    .await;
    let next = Box::pin(open_funded_request(
        &world, &mut payer, &mut shop, &key, "7",
    ))
    .await;
    let (held, _) = only_ledger(&payer).await;
    let entries = [first, next].map(|id| held.entries.iter().find(|e| e.attempt_id == id).unwrap());
    let ledger_key = payer.app().records_of("allowance_ledgers").await.unwrap()[0].key;
    let writes = world.record_writes(&world.alice, &ledger_key);
    let app_id = payer.app_id();

    Box::pin(write_owner_failure(&world, &mut payer, entries[0])).await;
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(world.record_writes(&world.alice, &ledger_key), writes + 1);
    assert_eq!(
        payer
            .app()
            .protocol_scan("paykit", b"lb/")
            .await
            .unwrap()
            .len(),
        1
    );
    let pending = Box::pin(payer.backup()).await.unwrap();
    drop(payer);

    let mut installed =
        Box::pin(world.load_on_clock(&world.alice, store.clone(), app_id, world.clock.clone()))
            .await;
    Box::pin(crate::common::finish_listing(&mut installed)).await;
    let usage = Box::pin(installed.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (usage.spent(), usage.released(), usage.reserved()),
        (0, 1, 1)
    );
    assert!(installed
        .app()
        .protocol_scan("paykit", b"lb/")
        .await
        .unwrap()
        .is_empty());
    let checkpoint = Box::pin(installed.backup()).await.unwrap();
    drop(installed);

    let mut payer = Box::pin(restore_funded_wallet(
        &world,
        crate::common::store(),
        &checkpoint,
    ))
    .await;
    Box::pin(finish_restored_reconciliation(&mut payer)).await;
    Box::pin(write_owner_failure(&world, &mut payer, entries[1])).await;
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let (after, revision) = only_ledger(&payer).await;
    assert!(after
        .entries
        .iter()
        .all(|e| e.outcome == Some(EntryOutcome::Failed)));
    assert_eq!(world.record_writes(&world.alice, &ledger_key), writes + 2);
    drop(payer);
    let mut restored = Box::pin(restore_funded_wallet(&world, store, &pending)).await;
    let usage = Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .expect("each of the two separate applied copies has its own evidence");
    assert_eq!(
        (
            usage.lifetime().as_str(),
            usage.spent(),
            usage.released(),
            usage.reserved()
        ),
        ("0", 0, 2, 0)
    );
    assert_eq!(only_ledger(&restored).await.1, revision);
    assert!(restored
        .app()
        .protocol_scan("paykit", b"lb/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(world.record_writes(&world.alice, &ledger_key), writes + 2);
    assert_eq!(
        (world.rail.pay_calls(&first), world.rail.pay_calls(&next)),
        (1, 1)
    );
}

async fn write_owner_failure(
    world: &World,
    payer: &mut paykit_sdk::Paykit,
    entry: &paykit_lib::allowances::LedgerEntry,
) {
    use pubky_dispatch_client::app::{ClaimOp, OpBatch};

    world.rail.settle(&entry.attempt_id, false);
    let claim = payer
        .app_mut()
        .read_claim(&entry.obligation)
        .await
        .unwrap()
        .unwrap();
    let failed = ClaimOp::failed(
        entry.obligation,
        claim.view.step,
        entry.attempt_id.to_bytes(),
    );
    payer
        .app_mut()
        .submit_batch(OpBatch::new().op(failed.kind()))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_executor_restores_applied_batch_after_predecessor_key_retirement() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::testing::Fault;

    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    for amount in ["5", "7"] {
        Box::pin(open_funded_request(
            &world, &mut payer, &mut shop, &key, amount,
        ))
        .await;
    }
    let (opened, _) = only_ledger(&payer).await;
    Box::pin(write_owner_failure(&world, &mut payer, &opened.entries[0])).await;
    let second = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let (base, predecessor) = only_ledger(&payer).await;
    assert_eq!(base.entries[0].outcome, Some(EntryOutcome::Failed));
    assert_eq!(predecessor.generation, second.generation());
    let ledger = payer.app().records_of("allowance_ledgers").await.unwrap()[0].key;
    let (_, base_revision) = applied_put(payer.app(), ledger, predecessor).await;

    let third = Box::pin(change_dropping(&world, &second, &mut payer, &[], [75; 32])).await;
    Box::pin(write_owner_failure(&world, &mut payer, &opened.entries[1])).await;
    third.fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    Box::pin(payer.receive(None)).await.unwrap();
    let markers = payer.app().protocol_scan("paykit", b"lb/").await.unwrap();
    assert_eq!(markers.len(), 1);
    let pending = Box::pin(payer.backup()).await.unwrap();
    let app_id = payer.app_id();
    drop(payer);

    let mut payer =
        Box::pin(world.load_on_clock(&world.alice, store.clone(), app_id, world.clock.clone()))
            .await;
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (usage.spent(), usage.released(), usage.reserved()),
        (0, 2, 0)
    );
    let (_, installed) = only_ledger(&payer).await;
    assert_eq!(installed.generation, third.generation());
    let (write, installed_revision) = applied_put(payer.app(), ledger, installed).await;
    assert_eq!(markers[0].1.as_slice(), write.body().batch.id.as_slice());
    let follows = |revision| Condition::RecordCurrent {
        key: ledger,
        revision,
        deleted: false,
    };
    assert!(write.body().conditions.contains(&follows(base_revision)));
    assert!(payer
        .app()
        .protocol_scan("paykit", b"lb/")
        .await
        .unwrap()
        .is_empty());
    let accepted = Box::pin(payer.backup()).await.unwrap();
    drop(payer);

    // A second installation advances the service one write beyond the installed batch.
    let mut other = Box::pin(restored_across_a_change(
        &world,
        &accepted,
        crate::common::store(),
        &key,
    ))
    .await;
    world.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let result = Box::pin(pays_under(&world, &mut shop, &mut other, &key, "1")).await;
    assert!(
        matches!(result.outcome, Ok(paykit_sdk::PayOutcome::Pending { .. })),
        "{result:?}"
    );
    let (_, successor) = only_ledger(&other).await;
    let (write, _) = applied_put(other.app(), ledger, successor).await;
    assert!(write
        .body()
        .conditions
        .contains(&follows(installed_revision)));
    let _moved = Box::pin(change_dropping(
        &world,
        &third,
        &mut other,
        &[predecessor.generation],
        [76; 32],
    ))
    .await;
    drop(other);
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, store),
        Default::default(),
        &pending,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"lb/")
            .await
            .unwrap(),
        markers
    );
    restored.app_mut().refresh_identity().await.unwrap();
    assert!(restored
        .app()
        .own_identity()
        .service_key_of(predecessor.generation)
        .is_none());
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    let usage = Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            usage.lifetime().as_str(),
            usage.spent(),
            usage.released(),
            usage.reserved()
        ),
        ("1", 0, 2, 1)
    );
    assert_eq!(only_ledger(&restored).await.1, successor);
    assert!(restored
        .app()
        .protocol_scan("paykit", b"lb/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_compacted_failure_paid_by_receipt_survives_reopen_and_restore() {
    paykit_sdk::with_early_ledger_compaction(Box::pin(compacted_failure_paid_by_receipt(false)))
        .await;
}

#[tokio::test]
async fn test_restored_compacted_failure_paid_by_receipt_copies_its_proof() {
    paykit_sdk::with_early_ledger_compaction(Box::pin(restored_compacted_failure_paid())).await;
}

/// A compacted failure a restored installation holds, paid after all and receipted: the
/// claim shows `done` and the payer record holds the payment's dated proof, which is copied
/// into the ledger's paid totals, and the Allowance counts it (Paykit spec 8.3, `failed`
/// releases).
async fn restored_compacted_failure_paid() {
    use paykit_lib::allowances::EntryOutcome;
    use paykit_sdk::rail::Behaviour;

    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(crate::common::store(), terms())).await;
    let identity = payer.identity();
    world.rail.set_history_payer(identity);
    let request = Box::pin(request_from(
        &mut shop,
        identity,
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Fail);
    let payment = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = payment else {
        panic!("{payment:?}");
    };
    let obligation = only_ledger(&payer).await.0.entries[0].obligation;
    assert!(world.rail.hold_history_outcome(&attempt_id));
    world.clock.advance_ms(60_001);
    let next = Box::pin(request_from(
        &mut shop,
        identity,
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(matches!(
        Box::pin(payer.pay_under_allowance(&next, &key))
            .await
            .outcome
            .unwrap(),
        paykit_sdk::PayOutcome::Paid { .. }
    ));
    let (compacted, compacted_version) = only_ledger(&payer).await;
    assert_eq!(compacted.failed.len(), 1);
    assert_eq!(compacted.failed[0].attempt_id, attempt_id);
    let backup = Box::pin(payer.backup()).await.unwrap();
    drop(payer);

    // Restored from that backup, the installation holds the compacted failure.
    let mut restored = Box::pin(restore_funded_wallet(
        &world,
        crate::common::store(),
        &backup,
    ))
    .await;
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    assert_eq!(only_ledger(&restored).await.1, compacted_version);

    // The failed payment completes after all; the seller receipts it, which marks the claim
    // done, and the payer record holds its proof.
    world.rail.complete_after_failure(&attempt_id);
    Box::pin(restored.resume_payments()).await.unwrap();
    Box::pin(crate::common::finish_listing(&mut shop)).await;
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.request_hash == Some(request.request_hash))
        .unwrap();
    Box::pin(shop.confirm_payment(&sale.record))
        .await
        .unwrap()
        .expect("seller receipt for the later settlement");
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    let claim = restored
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.view.attempt(&attempt_id.to_bytes()).unwrap().outcome,
        pubky_dispatch_core::claims::Outcome::Done
    );
    Box::pin(restored.resume_payments()).await.unwrap();
    let (copied, _) = only_ledger(&restored).await;
    assert!(copied.failed.is_empty(), "{copied:?}");
    assert_eq!(copied.spent.as_ref().unwrap().as_str(), "7");
    let usage = Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .expect("the copied proof establishes the ledger");
    assert_eq!(usage.lifetime().as_str(), "8");
    assert!(copied
        .entries
        .iter()
        .all(|e| e.outcome == Some(EntryOutcome::Done)));
}

#[tokio::test]
async fn test_executor_pays_after_copied_outcome_key_retirement() {
    paykit_sdk::with_early_ledger_compaction(Box::pin(compacted_failure_paid_by_receipt(true)))
        .await;
}

async fn compacted_failure_paid_by_receipt(retire_copy_key: bool) {
    use paykit_lib::allowances::EntryOutcome;
    use paykit_sdk::rail::{Behaviour, RailOutcome};
    use paykit_sdk::Rail;
    use pubky_dispatch_client::app::{Collection, RecordContent, RecordTarget};

    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    let identity = payer.identity();
    let app_id = payer.app_id();
    world.rail.set_history_payer(identity);
    let request = Box::pin(request_from(
        &mut shop,
        identity,
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Fail);
    let payment = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = payment else {
        panic!("{payment:?}");
    };
    let (failed, _) = only_ledger(&payer).await;
    let entry = &failed.entries[0];
    assert_eq!(
        (entry.attempt_id, entry.outcome),
        (attempt_id, Some(EntryOutcome::Failed))
    );
    assert!(world.rail.hold_history_outcome(&attempt_id));

    world.clock.advance_ms(60_001);
    let next = Box::pin(request_from(
        &mut shop,
        identity,
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(matches!(
        Box::pin(payer.pay_under_allowance(&next, &key))
            .await
            .outcome
            .unwrap(),
        paykit_sdk::PayOutcome::Paid { .. }
    ));
    let (compacted, compacted_version) = only_ledger(&payer).await;
    assert_eq!(compacted.count, 0);
    assert_eq!(compacted.entries.len(), 1);
    assert_eq!(compacted.failed.len(), 1);
    assert_eq!(compacted.failed[0].attempt_id, attempt_id);
    assert_eq!(compacted.failed[0].value.as_str(), "7");

    // The individual payment completes while the account-wide inventory is unavailable.
    // The seller issues the receipt and the payer applies it to the original failed claim.
    world.rail.set_history_payer(shop.identity());
    world.rail.complete_after_failure(&attempt_id);
    Box::pin(payer.resume_payments()).await.unwrap();
    Box::pin(crate::common::finish_listing(&mut shop)).await;
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.request_hash == Some(request.request_hash))
        .unwrap();
    Box::pin(shop.confirm_payment(&sale.record))
        .await
        .unwrap()
        .expect("seller receipt for the later settlement");
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let claim = payer
        .app_mut()
        .read_claim(&entry.obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.view.attempt(&attempt_id.to_bytes()).unwrap().outcome,
        pubky_dispatch_core::claims::Outcome::Done
    );
    assert_eq!(only_ledger(&payer).await.1, compacted_version);

    // The payer removes its completed activity. The claim now supplies the payment
    // evidence when the method resumes listing this attempt as Failed.
    let collection = paykit_lib::Collection::PayerActivity;
    let target = RecordTarget::new(
        Collection::new(collection.name(), collection.fixed_policy()),
        &paykit_lib::derive::natural_key::request_record(&shop.identity(), &request.request_hash),
    );
    let activity = payer.app().record_key(&target).unwrap();
    payer
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    let before_copy = Box::pin(payer.backup()).await.unwrap();
    // Only the copy into paid totals needs to be written in this generation.
    let copy_service = if retire_copy_key {
        Some(
            Box::pin(crate::allowances::advance_allowance_generation(
                &world, &mut payer,
            ))
            .await,
        )
    } else {
        None
    };
    world.rail.set_history_payer(identity);
    let inventory = world
        .rail
        .allowance_history(identity)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        inventory
            .iter()
            .find(|p| p.attempt_id == attempt_id)
            .unwrap()
            .outcome,
        RailOutcome::Failed
    );
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    assert_eq!(
        payer
            .app()
            .shared_record(&activity)
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );
    let (done, version) = only_ledger(&payer).await;
    assert_ne!(version, compacted_version);
    assert!(done.failed.is_empty());
    assert_eq!(
        (done.spent.as_ref().unwrap().as_str(), done.count),
        ("7", 1)
    );
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.lifetime().as_str(), "8");
    assert_eq!(usage.spent() + done.count, 2);
    if let Some(service) = copy_service {
        assert_eq!(version.generation, service.generation());
        assert_compaction_receipt(
            &payer,
            compacted_version.generation,
            version.generation,
            false,
        )
        .await;
        let next = Box::pin(change_dropping(&world, &service, &mut payer, &[], [75; 32])).await;
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "2")).await;
        assert_eq!(only_ledger(&payer).await.1.generation, next.generation());
        let _moved = Box::pin(change_dropping(
            &world,
            &next,
            &mut payer,
            &[version.generation],
            [76; 32],
        ))
        .await;
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "3")).await;
        assert_eq!(
            Box::pin(payer.reconcile_allowance(&key))
                .await
                .unwrap()
                .unwrap()
                .lifetime()
                .as_str(),
            "13"
        );
        return;
    }
    drop(payer);
    let mut reopened =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    Box::pin(reopened.receive(None)).await.unwrap();
    assert_eq!(
        Box::pin(reopened.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap(),
        usage
    );
    assert_eq!(only_ledger(&reopened).await.1, version);
    let backup = Box::pin(reopened.backup()).await.unwrap();
    drop(reopened);

    // This earlier backup predates the copied write. The service kept the terminal
    // view but retired its signed entries, and the payer removed its activity.
    let mut earlier = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, crate::common::store()),
        Default::default(),
        &before_copy,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut earlier)).await;
    let read = earlier
        .app_mut()
        .read_claim(&entry.obligation)
        .await
        .unwrap()
        .unwrap();
    assert!(read.view.tombstone && read.entries.is_empty());
    assert!(Box::pin(earlier.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_none());
    assert_eq!(only_ledger(&earlier).await.1, version);
    drop(earlier);

    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, crate::common::store()),
        Default::default(),
        &backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    assert_eq!(
        Box::pin(restored.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap(),
        usage
    );
    world.rail.refresh_history_outcome(&attempt_id);
    let inventory = world
        .rail
        .allowance_history(identity)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        inventory
            .iter()
            .find(|p| p.attempt_id == attempt_id)
            .unwrap()
            .outcome,
        RailOutcome::Paid(_)
    ));
    Box::pin(restored.receive(None)).await.unwrap();
    assert_eq!(
        Box::pin(restored.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap(),
        usage
    );
    assert_eq!(only_ledger(&restored).await, (done, version));
    use pubky_dispatch_client::app::{KeptReason, RecordRead};
    match restored.app_mut().read_record_fresh(&target).await.unwrap() {
        RecordRead::Fresh(record) => assert_eq!(record.content, RecordContent::Deleted),
        RecordRead::Kept {
            reason: KeptReason::NotFound,
            ..
        } => {}
        other => panic!("unexpected payer activity after deletion: {other:?}"),
    }
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_a_pending_attempt_that_fails_gives_its_amount_and_count_back() {
    // One payment a day, 10 in all.
    let terms = AllowanceTerms::from_json(&json!({
        "asset": "btc", "lifetime_amount_limit": "10",
        "period_limits": [{
            "amount_limit": null,
            "payment_count_limit": 1,
            "period": {"kind": "rolling", "every": 1, "unit": "day"},
        }],
        "per_payment_amount": null, "active_from": null, "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))
    .unwrap();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(crate::common::store(), terms)).await;
    world.rail.set_history_payer(payer.identity());
    // The method takes the 7 and leaves them pending: reserved, amount and count.
    let attempt_id = Box::pin(open_funded_request(
        &world, &mut payer, &mut shop, &key, "7",
    ))
    .await;
    let (reserved, _) = only_ledger(&payer).await;
    let entry = reserved.entries[0].clone();
    assert_eq!((entry.attempt_id, entry.outcome), (attempt_id, None));
    let next = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let waits = Box::pin(payer.pay_under_allowance(&next, &key))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(
        waits.refusal(),
        Some(&paykit_sdk::Refusal::NotNow(
            paykit_sdk::NotNowReason::AllowanceLimit {
                limit: "period_count".into()
            }
        )),
        "{waits:?}"
    );

    // The method answers for good that the payment failed: the ledger records it on the
    // same entry, and the reservation comes back, amount and count (Paykit spec 8.3,
    // `failed` releases).
    world.rail.settle(&attempt_id, false);
    Box::pin(payer.resume_payments()).await.unwrap();
    let (failed, _) = only_ledger(&payer).await;
    assert_eq!(failed.allowance_id, reserved.allowance_id);
    assert_eq!(failed.entries.len(), 1);
    let recorded = &failed.entries[0];
    assert_eq!(
        (recorded.attempt_id, recorded.obligation, recorded.outcome),
        (attempt_id, entry.obligation, Some(EntryOutcome::Failed))
    );
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            usage.lifetime().as_str(),
            usage.periods()[0].count(),
            usage.released()
        ),
        ("0", 0, 1)
    );
    // The 5 that waited fits now, in the same period.
    Box::pin(payer.pay_under_allowance(&next, &key))
        .await
        .outcome
        .unwrap();
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            usage.lifetime().as_str(),
            usage.periods()[0].count(),
            usage.spent(),
            usage.released()
        ),
        ("5", 1, 1, 1)
    );
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_a_proof_the_payer_record_cant_be_read_for_keeps_the_payment_counted() {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_dispatch_client::testing::Fault;

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = failed else {
        panic!("{failed:?}");
    };
    let obligation = only_ledger(&payer).await.0.entries[0].obligation;
    assert!(world.rail.hold_history_outcome(&attempt_id));
    world
        .clock
        .advance_ms(32 * pubky_dispatch_core::records::DAY_MS);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(payer
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    // The failed attempt paid after all, and the proof lands in the payer record, but the
    // record can't be read again while the executor copies outcomes.
    let records = payer.app().records_of("payer_activity").await.unwrap();
    assert_eq!(records.len(), 1);
    let unreadable = || Fault::Unreachable {
        path: format!(
            "/records/{}",
            pubky_dispatch_core::enc::b64(&records[0].key)
        ),
        after: 0,
    };
    world.rail.complete_after_failure(&attempt_id);
    world.alice.switch().fail_next(unreadable());
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(
        only_ledger(&payer).await.0.entries[0].outcome,
        Some(EntryOutcome::Failed)
    );
    // This app's copy of the record holds the proof: the 7 stay counted, not released.
    world.alice.switch().fail_next(unreadable());
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((usage.lifetime().as_str(), usage.released()), ("7", 0));
    // Once the record reads again, the proof is copied into the ledger, once.
    Box::pin(payer.resume_payments()).await.unwrap();
    let (done, revision) = only_ledger(&payer).await;
    assert_eq!(done.entries[0].outcome, Some(EntryOutcome::Done));
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(only_ledger(&payer).await.1, revision);
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (usage.lifetime().as_str(), usage.spent(), usage.released()),
        ("7", 1, 0)
    );
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_resume_copies_the_payers_later_payment_proof_once() {
    use paykit_lib::allowances::EntryOutcome;

    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("7", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = failed else {
        panic!("{failed:?}");
    };
    assert_eq!(
        only_ledger(&payer).await.0.entries[0].outcome,
        Some(EntryOutcome::Failed)
    );
    // The failure gives the 7 back.
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((usage.lifetime().as_str(), usage.released()), ("0", 1));
    let obligation = only_ledger(&payer).await.0.entries[0].obligation;
    let failed_backup = Box::pin(payer.backup()).await.unwrap();
    assert!(world.rail.hold_history_outcome(&attempt_id));
    world
        .clock
        .advance_ms(32 * pubky_dispatch_core::records::DAY_MS);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(payer
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    world.rail.complete_after_failure(&attempt_id);
    Box::pin(payer.resume_payments()).await.unwrap();
    let (done, revision) = only_ledger(&payer).await;
    assert_eq!(done.entries[0].outcome, Some(EntryOutcome::Done));
    assert!(payer
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    let app_id = payer.app_id();
    drop(payer);
    let mut payer =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(only_ledger(&payer).await.1, revision);
    // The late payment counts the 7 spent again, once.
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (usage.lifetime().as_str(), usage.spent(), usage.released()),
        ("7", 1, 0)
    );
    assert_eq!(world.rail.executions(&attempt_id), 1);
    drop(payer);
    // An older backup has the accepted failed predecessor. Only the current,
    // independently dated payer proof explains the copy after its claim expired.
    let restored_store = crate::common::store();
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, restored_store.clone()),
        Default::default(),
        &failed_backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    assert_eq!(
        Box::pin(restored.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "7"
    );
    drop(restored);
    let mut reopened =
        Box::pin(world.load_on_clock(&world.alice, restored_store, app_id, world.clock.clone()))
            .await;
    assert_eq!(
        Box::pin(reopened.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap()
            .lifetime()
            .as_str(),
        "7"
    );
    assert_eq!(only_ledger(&reopened).await.1, revision);
}

#[tokio::test]
async fn test_executor_copies_another_wallets_confirmed_failure_before_handoff() {
    Box::pin(confirm_foreign_failure(ForeignFailure::BeforeHandoff)).await;
}

#[tokio::test]
async fn test_executor_copies_another_wallets_confirmed_failure_after_handoff() {
    Box::pin(confirm_foreign_failure(ForeignFailure::AfterHandoff)).await;
}

#[tokio::test]
async fn test_released_room_fits_a_payment_a_period_limit_refused_first() {
    // Both caps 100: the period limit refuses 95 first while the other wallet's 9 are
    // reserved; once their failure is copied, 95 fits both limits and 105 neither.
    let terms = AllowanceTerms::from_json(&json!({
        "asset": "btc", "lifetime_amount_limit": "100",
        "period_limits": [{
            "amount_limit": "100",
            "payment_count_limit": null,
            "period": {"kind": "rolling", "every": 1, "unit": "day"},
        }],
        "per_payment_amount": null, "active_from": null, "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))
    .unwrap();
    Box::pin(confirm_foreign_failure_under(
        ForeignFailure::BeforeHandoff,
        terms,
        "period_amount",
    ))
    .await;
}

#[tokio::test]
async fn test_restore_from_before_the_copy_counts_the_copied_failure_once() {
    Box::pin(confirm_foreign_failure(ForeignFailure::RestoredBeforeCopy)).await;
}

#[tokio::test]
async fn test_executor_recovers_the_copied_failure_after_a_lost_reply() {
    Box::pin(confirm_foreign_failure(ForeignFailure::LostCopy)).await;
}

#[derive(PartialEq)]
enum ForeignFailure {
    BeforeHandoff,
    LostCopy,
    AfterHandoff,
    RestoredBeforeCopy,
}

async fn confirm_foreign_failure(case: ForeignFailure) {
    Box::pin(confirm_foreign_failure_under(case, terms(), "lifetime")).await;
}

/// Another wallet of the user confirms the executor's reserved attempt failed (Paykit spec
/// 7.3), and the executor copies that failure into the ledger with no one acting, which gives
/// the attempt's room back once (Paykit spec 8.3, `failed` releases). Under `terms`,
/// `first_limit` refuses a payment that never fits.
async fn confirm_foreign_failure_under(
    case: ForeignFailure,
    terms: AllowanceTerms,
    first_limit: &str,
) {
    let storage = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(storage.clone(), terms)).await;
    world.rail.set_history_payer(payer.identity());
    let original = payer.app().cert().clone();
    world
        .clock
        .set_ms(original.expires_at() - paykit_sdk::PRE_HANDOFF_BUDGET_MS - 120_000);
    let mut other = Box::pin(world.wallet("second wallet")).await;
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("9", world.clock.now_ms()),
    ))
    .await;
    // Larger requests, sent now while the shop's certificate holds, paid only later: one
    // that fits once the 9 come back, and one that never fits.
    let lasting = |value: &str| {
        draft(value, world.clock.now_ms())
            .with_proposal_expires_at_ms(world.clock.now_ms() + 3_650 * 24 * HOUR_MS)
    };
    let larger = Box::pin(request_from(&mut shop, payer.identity(), lasting("95"))).await;
    let too_large = Box::pin(request_from(&mut shop, payer.identity(), lasting("105"))).await;
    Box::pin(payer.receive(None)).await.unwrap();
    if case == ForeignFailure::AfterHandoff {
        world.rail.next(paykit_sdk::rail::Behaviour::Hang);
        Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap();
    } else {
        world
            .alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::LoseAnswer {
                path: "/ops".into(),
            });
        let lost = Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap_err();
        assert!(lost.kept_batch().is_some(), "{lost:?}");
    }
    Box::pin(payer.receive(None)).await.unwrap();
    let (held, _) = only_ledger(&payer).await;
    let entry = held.entries[0].clone();
    let handed_off = u64::from(case == ForeignFailure::AfterHandoff);
    assert_eq!(world.rail.executions(&entry.attempt_id), handed_off);
    // While the 9 are reserved, 95 waits for them to come back, refused by the limit that
    // fails first.
    let waits = Box::pin(payer.pay_under_allowance(&larger, &key))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(
        waits.refusal(),
        Some(&paykit_sdk::Refusal::NotNow(
            paykit_sdk::NotNowReason::AllowanceLimit {
                limit: first_limit.into()
            }
        )),
        "{waits:?}"
    );
    let before_copy = Box::pin(payer.backup()).await.unwrap();

    // The executor's certificate lapses, so the other wallet answers for the attempt.
    world.clock.set_ms(original.expires_at() + 1);
    Box::pin(other.receive(None)).await.unwrap();
    Box::pin(other.resolve_attempt(
        &entry.obligation,
        entry.attempt_id,
        paykit_sdk::Resolution::Failed,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let scopes = AppScopes::new().allowance_execute().build().unwrap();
    let id = payer.app_id();
    let renewed = crate::certificate::renew(&world, id, &scopes, 0);
    crate::certificate::list(&world, id, &original, &renewed);
    if case == ForeignFailure::LostCopy {
        world
            .alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::LoseAnswerAfter {
                path: "/ops".into(),
                // The receive's third write is the ledger copy.
                after: 2,
            });
    }
    // The receive that takes the renewed certificate copies the failure.
    let ledger_key = payer.app().records_of("allowance_ledgers").await.unwrap()[0].key;
    let writes = world.record_writes(&world.alice, &ledger_key);
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(world.record_writes(&world.alice, &ledger_key), writes + 1);
    if case == ForeignFailure::LostCopy {
        // The copy's answer was lost and the receive went on: the service applied it, and a
        // reopened wallet finds the write and counts it once.
        assert_eq!(only_ledger(&payer).await.0.entries[0].outcome, None);
        drop(payer);
        payer =
            Box::pin(world.load_on_clock(&world.alice, storage.clone(), id, world.clock.clone()))
                .await;
        Box::pin(payer.receive(None)).await.unwrap();
    }
    assert_eq!(
        payer.app().cert().hash(),
        renewed.hash(),
        "the renewed certificate was taken"
    );
    let (copied, revision) = only_ledger(&payer).await;
    assert_eq!(copied.entries[0].outcome, Some(EntryOutcome::Failed));
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((usage.lifetime().as_str(), usage.released()), ("0", 1));
    // Copied once: a later receive and resume write nothing more.
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(only_ledger(&payer).await.1, revision);
    assert_eq!(world.record_writes(&world.alice, &ledger_key), writes + 1);
    assert_eq!(world.rail.executions(&entry.attempt_id), handed_off);

    if case == ForeignFailure::RestoredBeforeCopy {
        // A backup taken before the copy restores to the same count: the other wallet's
        // verified claim explains the copied write.
        drop(payer);
        let mut restored = Box::pin(paykit_sdk::Paykit::restore(
            world.env(&world.alice, crate::common::store()),
            Default::default(),
            &before_copy,
            world.rail.clone(),
            world.timer(),
        ))
        .await
        .unwrap();
        Box::pin(crate::common::finish_listing(&mut restored)).await;
        let usage = Box::pin(restored.reconcile_allowance(&key))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((usage.lifetime().as_str(), usage.released()), ("0", 1));
        assert_eq!(only_ledger(&restored).await.1, revision);
        return;
    }

    // The room is back: 95 fits, and 105 never does.
    let shop_old = shop.app().cert().clone();
    let shop_id = shop.app_id();
    let shop_scopes = AppScopes::new().allowance().request().build().unwrap();
    let shop_renewed =
        crate::certificate::renew_for(&world, &world.bob, "shop", shop_id, &shop_scopes, 0);
    crate::certificate::list_for(&world.bob, shop_id, &shop_old, &shop_renewed);
    for _ in 0..2 {
        Box::pin(shop.receive(None)).await.unwrap();
    }
    assert_eq!(shop.app().cert().hash(), shop_renewed.hash());
    let refused = Box::pin(payer.pay_under_allowance(&too_large, &key))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&paykit_sdk::Refusal::NotAutomatic(format!(
            "the {first_limit} limit has no room"
        ))),
        "{refused:?}"
    );
    Box::pin(payer.pay_under_allowance(&larger, &key))
        .await
        .outcome
        .unwrap();
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((usage.lifetime().as_str(), usage.released()), ("95", 1));
}

#[tokio::test]
async fn test_host_can_show_the_ledger_alarm_after_reopening_the_wallet() {
    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(payer.ledger_alarm().is_none());
    let alarm = payer
        .app_mut()
        .raise_rollback(
            "paykit",
            "the current service is missing the executor's accepted permanent ledger",
        )
        .await
        .unwrap();
    assert_eq!(payer.ledger_alarm().as_ref(), Some(&alarm));

    let app_id = payer.app_id();
    drop(payer);
    let mut payer =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    assert_eq!(payer.ledger_alarm().as_ref(), Some(&alarm));
    assert!(Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .is_err());
    assert!(Box::pin(payer.pay_request(&request, Default::default()))
        .await
        .outcome
        .is_err());
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_counts_the_rounded_transfer_at_the_lifetime_limit_after_reopen() {
    let store = crate::common::store();
    let mut limits = terms();
    limits.lifetime_amount_limit = Some(paykit_lib::Decimal::new("1.999999995").unwrap());
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), limits)).await;
    world.rail.set_history_payer(payer.identity());

    for (requested, transferred) in [("1", "1"), ("0.4999999991", "0.5")] {
        let request = Box::pin(request_from(
            &mut shop,
            payer.identity(),
            draft(requested, world.clock.now_ms()),
        ))
        .await;
        Box::pin(payer.receive(None)).await.unwrap();
        let outcome = Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap();
        let paykit_sdk::PayOutcome::Paid { attempt_id, .. } = outcome else {
            panic!("the payment did not complete");
        };
        assert_eq!(
            world.rail.paid_amount(&attempt_id),
            Some(crate::common::btc(transferred))
        );
    }
    let app_id = payer.app_id();
    drop(payer);
    let mut payer =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.lifetime().as_str(), "1.5");

    let over = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("0.499999991", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let before = world.rail.all_executions();
    let error = Box::pin(payer.pay_under_allowance(&over, &key))
        .await
        .outcome
        .unwrap_err();
    assert!(error.to_string().contains("lifetime limit"), "{error}");
    assert_eq!(world.rail.all_executions(), before);

    let fits = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("0.49999999", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let outcome = Box::pin(payer.pay_under_allowance(&fits, &key))
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("the payment within the limit did not complete");
    };
    assert_eq!(
        world.rail.paid_amount(&attempt_id),
        Some(crate::common::btc("0.49999999"))
    );
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.lifetime().as_str(), "1.99999999");
    assert_eq!(world.rail.all_executions().len(), 3);
}

#[tokio::test]
async fn test_allowance_refuses_an_earlier_failed_attempt_outside_its_ledger() {
    use pubky_dispatch_client::app::{ClaimOp, OpBatch};

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let draft = draft("5", world.clock.now_ms());
    let deadline = draft
        .proposal_expires_at_ms
        .map(pubky_dispatch_core::claims::attempt_deadline);
    let request = Box::pin(request_from(&mut shop, world.alice_id(), draft)).await;
    Box::pin(payer.receive(None)).await.unwrap();
    let obligation = paykit_lib::derive::request_obligation(
        payer.app().naming_key(),
        &request.payee,
        &request.request_hash,
        0,
    );
    let attempt = [7; 16];
    for op in [
        ClaimOp::attempt(obligation, 0, attempt, 300, deadline),
        ClaimOp::failed(obligation, 1, attempt),
    ] {
        payer
            .app_mut()
            .submit_batch(OpBatch::new().op(op.kind()))
            .await
            .unwrap();
    }
    let error = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            error.refusal(),
            Some(paykit_sdk::Refusal::HeldForUser(
                paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(_)
            ))
        ),
        "{error}"
    );
    let claim = payer
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.step, 2);
    assert!(world.rail.all_executions().is_empty());
    assert!(matches!(
        Box::pin(payer.pay_request(&request, paykit_sdk::PayOptions::default()))
            .await
            .outcome
            .unwrap(),
        paykit_sdk::PayOutcome::Paid { .. }
    ));
}

fn repeat_without_method_history(world: &World) {
    world.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true)
        .with_idempotency_retention_ms(86_400_000),
    );
}

#[tokio::test]
async fn test_an_earlier_failure_under_the_same_allowance_allows_a_new_attempt() {
    use paykit_sdk::{rail::Behaviour, PayOutcome};
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        world.alice_id(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Fail);
    let previous = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(previous, PayOutcome::Failed { .. }),
        "{previous:?}"
    );
    let next = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    assert!(matches!(next, PayOutcome::Paid { .. }), "{next:?}");
    assert_ne!(next.attempt_id(), previous.attempt_id());
}

#[tokio::test]
async fn test_repeat_of_a_hand_payment_on_a_request_associated_with_an_allowance() {
    use paykit_sdk::rail::Behaviour;
    use paykit_sdk::{PayOptions, PayOutcome};

    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        world.alice_id(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Fail);
    let failed = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    assert!(matches!(failed, PayOutcome::Failed { .. }), "{failed:?}");
    world.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(payer.pay_request(&request, PayOptions::default()))
            .await
            .outcome
            .unwrap()
    else {
        panic!("hand payment was not pending")
    };
    repeat_without_method_history(&world);
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt_id), 2);
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[derive(Clone, Copy)]
enum AllowanceChange {
    Conflict,
    Handoff,
    End,
}

async fn repeat_after(change: AllowanceChange) {
    use paykit_sdk::rail::Behaviour;
    use paykit_sdk::PayOutcome;
    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        world.alice_id(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap()
    else {
        panic!("the Allowance payment was not pending")
    };
    match change {
        AllowanceChange::Conflict => {
            let held = payer.app().shared_record(&key).await.unwrap().unwrap();
            let record = paykit_lib::records::AllowanceRecord::read(
                held.content.value().unwrap(),
                &payer.identity(),
            )
            .unwrap();
            world.inject(
                &world.bob,
                "shop",
                &world.alice,
                &[payer.app_id()],
                "paykit.allowance_rejection",
                vec![record.proposal.hash()],
                json!({"allowance_id": record.allowance_id.as_str()})
                    .as_object()
                    .cloned()
                    .unwrap(),
            );
            Box::pin(payer.receive(None)).await.unwrap();
            assert_eq!(
                payer.allowance(&key).await.unwrap().status,
                AllowanceStatus::Conflicted
            );
        }
        AllowanceChange::Handoff => {
            let mut other = Box::pin(world.join(
                &world.alice,
                "second executor",
                AppScopes::new().allowance_execute().build().unwrap(),
            ))
            .await;
            Box::pin(other.receive(None)).await.unwrap();
            let held = other.app().shared_record(&key).await.unwrap().unwrap();
            let mut record = paykit_lib::records::AllowanceRecord::read(
                held.content.value().unwrap(),
                &other.identity(),
            )
            .unwrap();
            let terms = record.terms_hash().unwrap();
            let generation = other.app().own_identity().generation();
            let fields = paykit_lib::grant::grant_fields(
                &other.identity(),
                &record.counterparty,
                &record.allowance_id,
                &terms,
                generation,
                held.version,
            );
            let parts = fields.iter().map(Vec::as_slice).collect::<Vec<_>>();
            let grant = paykit_lib::ExecutorGrant {
                app_id: other.app_id(),
                terms_hash: terms,
                confirmed_generation: generation,
                cert: other.app().cert().signed().clone(),
                sig: other
                    .app()
                    .sign_protocol(paykit_lib::grant::EXECUTOR_GRANT_LABEL, &parts)
                    .unwrap(),
                origin: Default::default(),
            };
            record.grants.push(paykit_lib::records::GrantEntry {
                built_on: held.version,
                grant: grant.clone(),
                origin: Default::default(),
            });
            record.grant = Some(grant);
            let target =
                crate::allowances::allowances_target(&record.counterparty, &record.allowance_id);
            let write = other
                .app_mut()
                .record_op(&target, Some(&record.to_json().unwrap()), vec![])
                .await
                .unwrap();
            other
                .app_mut()
                .submit_batch(pubky_dispatch_client::app::OpBatch::new().entry(write))
                .await
                .unwrap();
            crate::allowances::notice(
                &world,
                &world.alice,
                "second executor",
                payer.app_id(),
                &key,
                "handed_off",
            );
            Box::pin(payer.receive(None)).await.unwrap();
            assert_eq!(
                payer.allowance(&key).await.unwrap().executor_app_id,
                Some(other.app_id())
            );
        }
        AllowanceChange::End => {
            let accepted = shop.allowances().await.unwrap().pop().unwrap();
            Box::pin(shop.end_allowance(&accepted.record))
                .await
                .unwrap();
            Box::pin(payer.receive(None)).await.unwrap();
            assert_eq!(
                payer.allowance(&key).await.unwrap().status,
                AllowanceStatus::Ended
            );
        }
    }
    let app_id = payer.app_id();
    drop(payer);
    let mut payer =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    assert!(!payer.allowance(&key).await.unwrap().consented);
    repeat_without_method_history(&world);
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(
        world.rail.pay_calls(&attempt_id),
        if matches!(change, AllowanceChange::End) {
            1
        } else {
            2
        }
    );
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_started_allowance_payment_repeats_after_conflict() {
    Box::pin(repeat_after(AllowanceChange::Conflict)).await;
}

#[tokio::test]
async fn test_started_allowance_payment_repeats_after_handoff() {
    Box::pin(repeat_after(AllowanceChange::Handoff)).await;
}

#[tokio::test]
async fn test_started_allowance_payment_stays_open_after_end() {
    Box::pin(repeat_after(AllowanceChange::End)).await;
}

#[tokio::test]
async fn test_started_allowance_payment_requires_both_confirmations_after_an_inbox_gap() {
    use paykit_sdk::{rail::Behaviour, PayOutcome};
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        world.alice_id(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap()
    else {
        panic!("the Allowance payment was not pending");
    };
    let now = world.clock.now_ms();
    world.alice.switch().push_missed(1, now - 1_000, now);
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let held = Box::pin(crate::common::held(&payer, &request)).await;
    Box::pin(payer.confirm_possibly_cancelled(&request, &held, UserConfirmed::new()))
        .await
        .unwrap();
    assert!(!payer.allowance(&key).await.unwrap().consented);
    repeat_without_method_history(&world);
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    assert!(payer.allowance(&key).await.unwrap().consented);
    // A restored or interrupted local store must still establish the original consent.
    use pubky_dispatch_client::app::LocalWrite;
    let consent_key = format!("ac/{}", pubky_dispatch_core::enc::b64(&key)).into_bytes();
    let consent = payer
        .app()
        .protocol_get("paykit", &consent_key)
        .await
        .unwrap()
        .unwrap();
    payer
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Delete {
                key: consent_key.clone(),
            }],
        )
        .await
        .unwrap();
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
    payer
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: consent_key,
                value: consent,
            }],
        )
        .await
        .unwrap();

    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt_id), 2);
    assert_eq!(world.rail.executions(&attempt_id), 1);
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
    assert!(Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&offer.record).await.unwrap().hold,
        UserConfirmed::new()
    ))
    .await
    .is_err());
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
            .fail_next(pubky_dispatch_client::testing::Fault::LoseAnswer {
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

/// A payer wallet that proposed an Allowance with automatic execution, approved by its
/// user, and the shop's acceptance read, the first grant not made yet.
async fn accepted_not_granted(
    store: std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>,
) -> (World, paykit_sdk::Paykit, paykit_sdk::Paykit, [u8; 32]) {
    Box::pin(accepted_not_granted_with(
        store,
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await
}

/// As [`accepted_not_granted`], the payer's app holding `scopes`.
async fn accepted_not_granted_with(
    store: std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>,
    scopes: paykit_sdk::RequestedScopes,
) -> (World, paykit_sdk::Paykit, paykit_sdk::Paykit, [u8; 32]) {
    let world = World::new();
    let mut payer =
        Box::pin(world.join_with(&world.alice, "wallet", scopes, Default::default(), store)).await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    Box::pin(payer.propose_executable_allowance(shop.identity(), terms(), UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let key = payer.allowances().await.unwrap().pop().unwrap().record;
    (world, payer, shop, key)
}

/// Whether `payer` holds its first grant's checkpoint (`lv/`) for the Allowance at
/// `record`: the local write of the grant's batch, applied when the batch is.
async fn granted(payer: &paykit_sdk::Paykit, record: &[u8; 32]) -> bool {
    let key = format!("lv/{}", pubky_dispatch_core::enc::b64(record));
    payer
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .is_some()
}

async fn missed_item(world: &World, payer: &mut paykit_sdk::Paykit) {
    let now = world.clock.now_ms();
    world.alice.switch().push_missed(1, now - 1_000, now);
    Box::pin(payer.receive(None)).await.unwrap();
    // The copy of the shared records counts as listed again from the next receive's listing.
    Box::pin(payer.receive(None)).await.unwrap();
}

#[tokio::test]
async fn test_executor_approval_before_a_missed_item_needs_a_new_confirmation() {
    // Every Allowance is held at a `missed` item, one approved but not granted yet among
    // them (Paykit spec 7.2): its first grant waits for the user to confirm again.
    let (world, mut payer, _shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    Box::pin(missed_item(&world, &mut payer)).await;
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the approval was given before the missed item");
    assert!(!payer.allowance(&key).await.unwrap().consented);
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    assert!(payer.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_kept_first_grant_landing_after_a_missed_item_consents_nothing() {
    // The first grant's batch goes unanswered and is kept; a `missed` item is read before it
    // lands. The grant applies, but the batch carries no consent, and the approval it was
    // made on is the earlier hold's: the Allowance waits for the user to confirm it again.
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, _shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is unanswered");
    world.clock.advance_ms(1_000);
    // The kept batch can't be sent while the item is read.
    world.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let now = world.clock.now_ms();
    world.alice.switch().push_missed(1, now - 1_000, now);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(
        !Box::pin(granted(&payer, &key)).await,
        "the grant's batch is still kept"
    );
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(
        Box::pin(granted(&payer, &key)).await,
        "the kept batch landed after the missed item"
    );
    Box::pin(payer.receive(None)).await.unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(
        view.executor_app_id,
        Some(payer.app_id()),
        "the grant applied"
    );
    assert!(!view.consented, "{view:?}");
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("no consent without a confirmation under the current hold");
    assert!(!payer.allowance(&key).await.unwrap().consented);
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    assert!(payer.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_notice_before_the_consent_step_is_kept_as_a_removal() {
    // The first grant's batch goes unanswered and is kept. Another own app ends the
    // Allowance, and its notice is read once the kept batch applied, before the consent is
    // written. The notice is kept as a removal, so a later resume consents nothing, although
    // the record still shows the Allowance running (Paykit spec 8.3).
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, _shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    let _phone = Box::pin(world.join(
        &world.alice,
        "phone",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is unanswered");
    Box::pin(payer.receive(None)).await.unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(
        view.executor_app_id,
        Some(payer.app_id()),
        "the grant applied"
    );
    assert!(!view.consented, "{view:?}");
    crate::allowances::notice(&world, &world.alice, "phone", payer.app_id(), &key, "ended");
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&payer, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the notice removed the Allowance");
    assert!(!payer.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_refused_confirmation_leaves_no_approval() {
    // After a `missed` item the user confirms the Allowance while its counterparty is
    // blocked: the confirmation is refused and leaves no approval behind, so once unblocked
    // a resume alone doesn't grant it (Paykit spec 7.2, 8.3).
    let (world, mut payer, shop, key) = Box::pin(accepted_not_granted_with(
        crate::common::store(),
        AppScopes::new()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    Box::pin(missed_item(&world, &mut payer)).await;
    // The user is shown the Allowance, then it is blocked before the confirmation.
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let shown = payer.allowance(&key).await.unwrap().hold;
    payer.block(&shop.identity()).await.unwrap();
    let refused = Box::pin(payer.confirm_allowance_executor(&shown, UserConfirmed::new()))
        .await
        .expect_err("the counterparty is blocked");
    assert_eq!(
        refused.refusal(),
        Some(&paykit_sdk::Refusal::HeldForUser(
            paykit_sdk::HeldForUserReason::CounterpartyBlocked("the counterparty is blocked")
        )),
        "{refused:?}"
    );
    payer.unblock(&shop.identity()).await.unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the refused confirmation approved nothing");
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(view.executor_app_id, None, "{view:?}");
    assert!(!view.consented);
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    assert!(payer.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_refused_resume_after_a_restore_consents_nothing() {
    // The first grant's batch is kept and applies before its consent is written, and a
    // backup is taken then, after the request it pays was read, so its restore is clean.
    // The original pays under the Allowance, so the ledger moves on. Restored from that
    // backup, the executor can't reconcile the ledger without the method's complete
    // history: its resume and its confirmation are refused at the ledger check, and leave
    // no consent behind (Paykit spec 8.3, "A grant implies a ledger").
    use paykit_sdk::PayOutcome;
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, mut shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        world.alice_id(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is unanswered");
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(Box::pin(granted(&payer, &key)).await, "the grant applied");
    let backup = payer.backup().await.unwrap();
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let paid = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    drop(payer);
    // The method's history of that payment is gone: the ledger can't be reconciled.
    world.rail.forget_history(&paid.attempt_id());
    let mut restored = Box::pin(restored_clean(&world, &backup)).await;
    Box::pin(restored.receive(None)).await.unwrap();
    Box::pin(reconciled(&mut restored)).await;
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(
        Box::pin(approvals(&restored)).await.len(),
        1,
        "the clean restore kept the approval, so the ledger check is what refuses"
    );
    Box::pin(restored.resume_allowance_executor(&key))
        .await
        .expect_err("the ledger can't be reconciled");
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&restored, &key)).await,
        None
    );
    Box::pin(restored.confirm_allowance_executor(
        &restored.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .expect_err("a confirmation doesn't reconcile the ledger either");
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&restored, &key)).await,
        None
    );
}

/// Run `reconcile` until its listing of the shared records completes.
async fn reconciled(app: &mut paykit_sdk::Paykit) {
    for _ in 0..8 {
        match Box::pin(app.reconcile()).await {
            Ok(_) => return,
            Err(error) => assert!(
                matches!(
                    error.refusal(),
                    Some(paykit_sdk::Refusal::NotNow(
                        paykit_sdk::NotNowReason::RecordsUnlisted {
                            retry: paykit_sdk::ListingRetry::Reconcile
                        }
                    ))
                ),
                "{error:?}"
            ),
        }
    }
    panic!("the listing didn't complete");
}

/// `payer`'s approvals of automatic execution (`xa/`), raw.
async fn approvals(payer: &paykit_sdk::Paykit) -> Vec<(Vec<u8>, Vec<u8>)> {
    payer.app().protocol_scan("paykit", b"xa/").await.unwrap()
}

/// `backup` restored into a new store, with a cursor the service reports clean.
async fn restored_clean(world: &World, backup: &paykit_sdk::PaykitBackup) -> paykit_sdk::Paykit {
    paykit_sdk::Paykit::restore(
        world.env(&world.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        backup,
        std::sync::Arc::clone(&world.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        world.timer(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_executor_restored_approval_dropped_by_a_missed_item_before_the_verdict() {
    // A restore carries the backup's approval to the verdict. A `missed` item read before a
    // clean verdict holds every Allowance (Paykit spec 7.2): the carried approval is dropped
    // with it, and a resume doesn't grant without a new confirmation.
    let (world, mut payer, _shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = Box::pin(restored_clean(&world, &backup)).await;
    Box::pin(missed_item(&world, &mut restored)).await;
    Box::pin(reconciled(&mut restored)).await;
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(restored.resume_allowance_executor(&key))
        .await
        .expect_err("the approval was given before the missed item");
    assert!(!restored.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_backup_approval_under_an_older_hold_is_not_carried() {
    // An approval kept under a hold that a `missed` item ended, still in the backup, is not
    // the backup's own hold's: a clean restore carries nothing from it (Paykit spec 7.2).
    let (world, mut payer, _shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    let before = Box::pin(approvals(&payer)).await;
    assert_eq!(before.len(), 1, "one approval under the first hold");
    Box::pin(missed_item(&world, &mut payer)).await;
    assert!(Box::pin(approvals(&payer)).await.is_empty());
    let (stale, value) = before.into_iter().next().unwrap();
    payer
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_dispatch_client::app::LocalWrite::Put { key: stale, value }],
        )
        .await
        .unwrap();
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = Box::pin(restored_clean(&world, &backup)).await;
    Box::pin(restored.receive(None)).await.unwrap();
    Box::pin(restored.reconcile()).await.unwrap();
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(restored.resume_allowance_executor(&key))
        .await
        .expect_err("the approval was under an older hold");
    assert!(!restored.allowance(&key).await.unwrap().consented);
}

/// The grants the Allowance record at `record` holds, as the payer reads it.
async fn grants(payer: &paykit_sdk::Paykit, record: &[u8; 32]) -> usize {
    let held = payer.app().shared_record(record).await.unwrap().unwrap();
    AllowanceRecord::read(held.content.value().unwrap(), &payer.identity())
        .unwrap()
        .grants
        .len()
}

#[tokio::test]
async fn test_executor_resume_retried_without_a_read_lands_a_kept_grant() {
    // The first grant's batch applies at the service and its answer is lost, or never
    // reaches it. The host retries the resume, with no read in between: the retry sends
    // the kept batch again, builds no second grant, and consents (Paykit spec 8.3).
    use pubky_dispatch_client::testing::Fault;
    for applied in [true, false] {
        let (world, mut payer, mut shop, key) =
            Box::pin(accepted_not_granted(crate::common::store())).await;
        world.alice.switch().fail_next(if applied {
            Fault::LoseAnswer {
                path: "/ops".into(),
            }
        } else {
            Fault::Unreachable {
                path: "/ops".into(),
                after: 0,
            }
        });
        Box::pin(payer.resume_allowance_executor(&key))
            .await
            .expect_err("the grant's batch is kept");
        assert!(!Box::pin(granted(&payer, &key)).await, "{applied}");
        // A request arrives meanwhile: the read that lands the grant takes it, and the
        // host's next receive still tells of it.
        Box::pin(request_from(
            &mut shop,
            world.alice_id(),
            draft("1", world.clock.now_ms()),
        ))
        .await;
        Box::pin(payer.resume_allowance_executor(&key))
            .await
            .unwrap_or_else(|error| panic!("{applied}: {error:?}"));
        assert!(Box::pin(granted(&payer, &key)).await, "{applied}");
        assert!(payer.allowance(&key).await.unwrap().consented, "{applied}");
        assert_eq!(Box::pin(grants(&payer, &key)).await, 1, "{applied}");
        let report = Box::pin(payer.receive(None)).await.unwrap();
        assert!(
            crate::allowances::received(&report, "paykit.payment_request"),
            "{applied}: {report:?}"
        );
    }
}

#[tokio::test]
async fn test_executor_notice_while_the_grant_batch_is_kept_is_kept_as_a_removal() {
    // The first grant's batch applies and its answer is lost. Another own app ends the
    // Allowance; its notice is read while the batch can't be sent again, before the grant's
    // checkpoint exists. The notice is kept as a removal all the same: once the batch lands,
    // a resume consents nothing (Paykit spec 8.3, "even before it has added it").
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, _shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    let _phone = Box::pin(world.join(
        &world.alice,
        "phone",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's answer is lost");
    crate::allowances::notice(&world, &world.alice, "phone", payer.app_id(), &key, "ended");
    world.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(
        !Box::pin(granted(&payer, &key)).await,
        "the batch is still kept"
    );
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&payer, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(Box::pin(granted(&payer, &key)).await, "the batch landed");
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the notice removed the Allowance");
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&payer, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_executor_end_seen_before_the_consent_is_kept_as_a_removal() {
    // The first grant's batch applies and its answer is lost. The shop ends the Allowance,
    // and the wallet's next read both lands the kept grant and reads the end, before any
    // consent exists. The end is kept as a removal: a resume consents nothing (Paykit spec
    // 8.3, "even before it has added it").
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, mut shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's answer is lost");
    let accepted = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.end_allowance(&accepted.record))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(
        Box::pin(granted(&payer, &key)).await,
        "the kept grant landed"
    );
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&payer, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the Allowance ended");
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&payer, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

async fn restored_approval(clean: bool) -> bool {
    let storage = crate::common::store();
    let (world, mut payer, _shop, key) = Box::pin(accepted_not_granted(storage)).await;
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = paykit_sdk::Paykit::restore(
        world.env(&world.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&world.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        world.timer(),
    )
    .await
    .unwrap();
    if clean {
        Box::pin(restored.receive(None)).await.unwrap();
    } else {
        crate::common::restore_with_unknown_cursor(&mut restored).await;
    }
    Box::pin(restored.reconcile()).await.unwrap();
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(restored.resume_allowance_executor(&key))
        .await
        .is_ok()
        && restored.allowance(&key).await.unwrap().consented
}

#[tokio::test]
async fn test_executor_clean_restore_keeps_the_approval() {
    assert!(Box::pin(restored_approval(true)).await);
}

#[tokio::test]
async fn test_executor_restore_that_isnt_clean_needs_a_new_confirmation() {
    assert!(!Box::pin(restored_approval(false)).await);
}

/// After a first grant, the consent list's entry for it is `consent`; a resume, which turns
/// an approval into consent, leaves the entry exactly as it is.
async fn promote_keeps(consent: serde_json::Value) {
    use pubky_dispatch_client::app::LocalWrite;
    let (_world, mut payer, _shop, key) = Box::pin(first_grant()).await;
    let at = format!("ac/{}", pubky_dispatch_core::enc::b64(&key));
    let held = payer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut entry: serde_json::Value = serde_json::from_slice(&held).unwrap();
    entry["d"]["consent"] = consent;
    let bytes = serde_json::to_vec(&entry).unwrap();
    payer
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: at.as_bytes().to_vec(),
                value: bytes.clone(),
            }],
        )
        .await
        .unwrap();
    let _ = Box::pin(payer.resume_allowance_executor(&key)).await;
    assert_eq!(
        payer
            .app()
            .protocol_get("paykit", at.as_bytes())
            .await
            .unwrap(),
        Some(bytes)
    );
    assert!(!payer.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_consent_never_replaces_a_removal() {
    Box::pin(promote_keeps(json!({ "removed": { "reason": "ended" } }))).await;
}

#[tokio::test]
async fn test_executor_consent_never_replaces_a_newer_decision() {
    Box::pin(promote_keeps(json!({ "held": { "confirm_again": true } }))).await;
}

/// An Allowance that expires at `expires`, its reservation written 5 s before and its batch's
/// answer taking `delay_ms`, so the first handoff is judged at `expires - 5_000 + delay_ms`.
/// Returns the payment's outcome, the executions and the ledger entry's outcome.
async fn handoff_at_expiry(delay_ms: u64) -> (paykit_sdk::PayResult, usize, String) {
    Box::pin(handoff_at_expiry_on(delay_ms, false)).await
}

/// As [`handoff_at_expiry`]; with `device_stopped`, the payer's device clock stays at the
/// reservation's time while the service's runs on, so only the verified service time can
/// pass the expiry.
async fn handoff_at_expiry_on(
    delay_ms: u64,
    device_stopped: bool,
) -> (paykit_sdk::PayResult, usize, String) {
    let expires = pubky_dispatch_client::testing::START_MS + 60 * 60_000;
    let terms = AllowanceTerms::from_json(&json!({
        "asset": "btc", "lifetime_amount_limit": "100", "period_limits": [],
        "per_payment_amount": null, "active_from": null,
        "expires_at": pubky_dispatch_core::time::rfc3339(expires),
        "allowed_payment_endpoint_identifiers": null
    }))
    .unwrap();
    let storage = crate::common::store();
    let (world, payer, mut shop, key) = Box::pin(first_grant_in(storage.clone(), terms)).await;
    world.rail.set_history_payer(payer.identity());
    let app_id = payer.app_id();
    drop(payer);
    let slow = std::sync::Arc::new(crate::common::SlowOps::new(std::sync::Arc::clone(
        &world.router,
    )));
    world.clock.set_ms(expires - 5_000);
    let mut env = world.env(&world.alice, storage);
    env.transport = std::sync::Arc::clone(&slow)
        as std::sync::Arc<dyn pubky_dispatch_client::transport::Transport>;
    if device_stopped {
        env.clock = std::sync::Arc::new(pubky_dispatch_client::clock::TestClock::new(
            expires - 5_000,
        )) as std::sync::Arc<dyn pubky_dispatch_client::clock::Clock>;
    }
    let mut payer = paykit_sdk::Paykit::load(
        env,
        paykit_sdk::PaykitConfig::default(),
        app_id,
        std::sync::Arc::clone(&world.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        world.timer(),
    )
    .await
    .unwrap();
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    slow.delay_next(std::sync::Arc::clone(&world.clock), delay_ms);
    let result = Box::pin(payer.pay_under_allowance(&request, &key)).await;
    let executions = world.rail.all_executions().len();
    let (ledger, _) = only_ledger(&payer).await;
    let outcome = format!("{:?}", ledger.entries.last().unwrap().outcome);
    (result, executions, outcome)
}

#[tokio::test]
async fn test_executor_expiry_after_the_reservation_refuses_the_first_handoff() {
    // The reservation is written 5 s before the expiry and its batch's answer takes 10 s:
    // the first handoff comes after the expiry, pays nothing, and the attempt is failed.
    let (result, executions, outcome) = Box::pin(handoff_at_expiry(10_000)).await;
    let refused = result.outcome.unwrap_err();
    assert!(
        format!("{refused:?}").contains("the Allowance expired before handoff"),
        "{refused:?}"
    );
    assert_eq!(executions, 0);
    assert!(outcome.contains("Failed"), "{outcome}");
}

#[tokio::test]
async fn test_executor_handoff_at_the_expiry_is_refused() {
    let (result, executions, outcome) = Box::pin(handoff_at_expiry(5_000)).await;
    let refused = result.outcome.unwrap_err();
    assert!(
        format!("{refused:?}").contains("the Allowance expired before handoff"),
        "{refused:?}"
    );
    assert_eq!(executions, 0);
    assert!(outcome.contains("Failed"), "{outcome}");
}

#[tokio::test]
async fn test_executor_handoff_refused_by_the_service_time_alone() {
    // The device clock stays 5 s before the expiry. The reservation's answer takes 5 s on
    // the service's clock, so the reads after it carry a service time at the expiry. The
    // later of the two clocks has reached the expiry, so the first handoff pays nothing; a
    // millisecond earlier it pays.
    let (result, executions, outcome) = Box::pin(handoff_at_expiry_on(5_000, true)).await;
    let refused = result.outcome.unwrap_err();
    assert!(
        format!("{refused:?}").contains("the Allowance expired before handoff"),
        "{refused:?}"
    );
    assert_eq!(executions, 0);
    assert!(outcome.contains("Failed"), "{outcome}");
    let (result, executions, _) = Box::pin(handoff_at_expiry_on(4_999, true)).await;
    assert!(
        matches!(result.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{:?}",
        result.outcome
    );
    assert_eq!(executions, 1);
}

#[tokio::test]
async fn test_executor_handoff_a_millisecond_before_the_expiry_pays() {
    let (result, executions, _) = Box::pin(handoff_at_expiry(4_999)).await;
    assert!(
        matches!(result.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{:?}",
        result.outcome
    );
    assert_eq!(executions, 1);
}

#[tokio::test]
async fn test_executor_expired_allowance_repeats_no_handoff() {
    // The first call went out and hung. The method then keeps no history for it, and its
    // idempotency lets the SDK repeat the call. Once the Allowance has expired, a resume
    // makes no further call: nothing more is paid under an expired Allowance (Paykit spec
    // 8.3). The attempt stays open as possibly paid, and its ledger entry stays reserved.
    use paykit_sdk::rail::Behaviour;
    use paykit_sdk::PayOutcome;
    let start = pubky_dispatch_client::testing::START_MS;
    let expires = start + 60 * 60_000;
    let terms = AllowanceTerms::from_json(&json!({
        "asset": "btc", "lifetime_amount_limit": "100", "period_limits": [],
        "per_payment_amount": null, "active_from": null,
        "expires_at": pubky_dispatch_core::time::rfc3339(expires),
        "allowed_payment_endpoint_identifiers": null
    }))
    .unwrap();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(crate::common::store(), terms)).await;
    world.rail.set_history_payer(payer.identity());
    world.clock.set_ms(expires - 30_000);
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap()
    else {
        panic!("the Allowance payment was not pending")
    };
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
    // The method's history can't tell whether the call went, and its idempotency allows a
    // repeat: only the Allowance's expiry stops it.
    world.rail.forget_history(&attempt_id);
    repeat_without_method_history(&world);
    world.clock.set_ms(expires + 1);
    let resumed = Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt_id), 1, "{resumed:?}");
    assert!(
        resumed.iter().any(|outcome| matches!(
            outcome,
            PayOutcome::Uncertain { attempt_id: id, .. } if *id == attempt_id
        )),
        "{resumed:?}"
    );
    let payment = payer
        .payments()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.attempt_id == attempt_id)
        .unwrap();
    assert_ne!(payment.stage, paykit_sdk::Stage::Finished);
    let ledger = payer
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ledger = Ledger::from_json(ledger.content.value().unwrap()).unwrap();
    let entry = ledger
        .entries
        .iter()
        .find(|entry| entry.attempt_id == attempt_id)
        .unwrap();
    assert_eq!(entry.outcome, None);
}

#[tokio::test]
async fn test_executor_holds_a_period_whose_record_diverges_after_paying_is_journaled() {
    use crate::period_payments::{diverge_at_paying, period_key, StageStore};
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};

    // Right after the executor journals the period's `Paying`, a view shows its period
    // record divergent: the handoff is held and the rail is never called (Paykit spec 6).
    let base = std::sync::Arc::new(StageStore::default());
    let storage = std::sync::Arc::clone(&base)
        as std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>;
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(storage, terms())).await;
    world.rail.set_history_payer(payer.identity());
    let start = world.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let period = PeriodRef::from(schedule.periods(1).unwrap()[0]);
    let req = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("70", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.accept_subscription_under_allowance(&req, &key))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let record = period_key(&payer, &req, period);
    diverge_at_paying(&base, payer.app_id(), record);
    let outcome = Box::pin(payer.pay_period_under_allowance(&req, &period))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Pending { .. }),
        "{outcome:?}"
    );
    assert!(world.rail.all_executions().is_empty());
    assert!(base.fired(), "the record diverged after Paying");
}

#[tokio::test]
async fn test_executor_restored_handoff_resolved_with_no_claim_keeps_the_allowance_usable() {
    // The executor's attempt is written to the claim but its answer is lost, so the payment
    // method is never called. Once the executor's certificate has expired, another own app
    // writes the attempt failed, and the executor, renewed, sees it and is backed up. Past
    // the claim's window the service drops the claim. Restored from the backup at the same
    // service, the attempt is open with no claim showing it, the method's history no longer
    // keeps it, and the user resolves it as not paid. The Allowance is then read and pays
    // (Paykit spec 7.2, 8.3, 8.4).
    let storage = crate::common::store();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(storage.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let original = payer.app().cert().clone();
    world
        .clock
        .set_ms(original.expires_at() - paykit_sdk::PRE_HANDOFF_BUDGET_MS - 120_000);
    let mut other = Box::pin(world.wallet("second wallet")).await;
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("9", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world
        .alice
        .switch()
        .fail_next(pubky_dispatch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let lost = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap_err();
    assert!(lost.kept_batch().is_some(), "{lost:?}");
    Box::pin(payer.receive(None)).await.unwrap();
    let (held, _) = only_ledger(&payer).await;
    let (obligation, attempt) = (held.entries[0].obligation, held.entries[0].attempt_id);
    world.clock.set_ms(original.expires_at() + 1);
    Box::pin(other.receive(None)).await.unwrap();
    Box::pin(other.resolve_attempt(
        &obligation,
        attempt,
        paykit_sdk::Resolution::Failed,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let scopes = AppScopes::new().allowance_execute().build().unwrap();
    let id = payer.app_id();
    let renewed = crate::certificate::renew(&world, id, &scopes, 0);
    crate::certificate::list(&world, id, &original, &renewed);
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.reconcile_allowance(&key)).await.unwrap();
    // The shop's certificate, issued with the payer's, is renewed too.
    let shop_scopes = AppScopes::new().allowance().request().build().unwrap();
    let shop_cert = shop.app().cert().clone();
    let shop_renewed =
        crate::certificate::renew_for(&world, &world.bob, "shop", shop.app_id(), &shop_scopes, 0);
    crate::certificate::list_for(&world.bob, shop.app_id(), &shop_cert, &shop_renewed);
    Box::pin(shop.receive(None)).await.unwrap();
    let backup = payer.backup().await.unwrap();
    drop(payer);
    // By the restore, the method's history keeps a handoff a week: it no longer tells.
    let declared = |retention: Option<u64>| {
        let declaration = paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_history(true)
        .with_history_survives_restore(true);
        match retention {
            Some(ms) => declaration.with_history_retention_ms(ms),
            None => declaration,
        }
    };
    world
        .rail
        .set_declaration(declared(Some(7 * 24 * crate::common::HOUR_MS)));
    world.clock.advance_ms(
        2 * 24 * crate::common::HOUR_MS + pubky_dispatch_core::claims::RECONCILIATION_MS,
    );
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, crate::common::store()),
        Default::default(),
        &backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    // Reconciliation lists the shared records in pages, asked again until done.
    let mut reconciled = Box::pin(restored.reconcile()).await;
    for _ in 0..8 {
        let Err(error) = &reconciled else { break };
        assert!(
            matches!(error.refusal(), Some(paykit_sdk::Refusal::NotNow(_))),
            "{error}"
        );
        reconciled = Box::pin(restored.reconcile()).await;
    }
    reconciled.unwrap();
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    assert!(restored
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    Box::pin(restored.resolve_attempt(
        &obligation,
        attempt,
        paykit_sdk::Resolution::Failed,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    // A history kept for good again, the Allowance is read and pays.
    world.rail.set_declaration(declared(None));
    assert!(Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_some());
    let next = Box::pin(request_from(
        &mut shop,
        restored.identity(),
        draft("2", world.clock.now_ms()),
    ))
    .await;
    Box::pin(restored.receive(None)).await.unwrap();
    let paid = Box::pin(restored.pay_under_allowance(&next, &key))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
}

/// A one-time request from the shop for `value` btc, its hold confirmed if it is held,
/// paid under the Allowance at `key`.
async fn pays_under(
    world: &World,
    shop: &mut paykit_sdk::Paykit,
    payer: &mut paykit_sdk::Paykit,
    key: &[u8; 32],
    value: &str,
) -> paykit_sdk::PayResult {
    let request = Box::pin(request_from(
        shop,
        payer.identity(),
        draft(value, world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    if let Some(held) = payer.request(&request).await.unwrap().held {
        Box::pin(payer.confirm_possibly_cancelled(&request, &held, UserConfirmed::new()))
            .await
            .unwrap();
    }
    Box::pin(payer.pay_under_allowance(&request, key)).await
}

#[tokio::test]
async fn test_executor_grant_before_an_orderly_generation_change_still_pays() {
    // The service's generation changes in an orderly way, with nothing declared lost: the
    // Allowance's grant and ledger, written in the earlier generation, keep their evidence,
    // so it goes on paying automatically, and after a `missed` item the user's confirmation
    // lands (Paykit spec 8.3, "After provider loss" pauses only after a loss; Pubky Dispatch
    // spec, Section 12.5).
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
    let before = payer.app().own_identity().generation();
    let service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    assert!(payer.app().own_identity().generation() > before);
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "2")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );

    // A `missed` item at the service of the new generation holds the Allowance.
    let now = world.clock.now_ms();
    service.push_missed(1, now - 1_000, now);
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    assert!(!payer.allowance(&key).await.unwrap().consented);
    let hold = payer.allowance(&key).await.unwrap().hold;
    Box::pin(payer.confirm_allowance_executor(&hold, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "3")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

/// A subscription from the shop, accepted under the Allowance at `key`: its refusal. After a
/// provider loss a one-time request is held for good under any Allowance (Paykit spec 8.1),
/// so a Billing Period shows the Allowance's own state.
async fn subscription_refused(
    world: &World,
    shop: &mut paykit_sdk::Paykit,
    payer: &mut paykit_sdk::Paykit,
    key: &[u8; 32],
) -> paykit_sdk::Refusal {
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
    let start = world.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let request = Box::pin(request_from(
        shop,
        payer.identity(),
        draft("1", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.accept_subscription_under_allowance(&request, key))
        .await
        .unwrap_err()
        .refusal()
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn test_executor_grant_before_a_provider_loss_pays_nothing() {
    // A provider loss is no orderly change: the Allowance's grant and ledger from before it
    // pay nothing automatically on the replacement service (Paykit spec 8.3, "After
    // provider loss").
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let _service = crate::provider_loss::declare_loss(&world);
    let uncertain_until = Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    world.clock.set_ms(
        uncertain_until.max(world.clock.now_ms()) + pubky_dispatch_client::app::DOC_CACHE_MS,
    );
    // The grant from before the loss pays nothing: the user confirms the Allowance again
    // under the loss's hold, and its usage waits for repair (Paykit spec 8.3, "After
    // provider loss").
    let refused = Box::pin(subscription_refused(&world, &mut shop, &mut payer, &key)).await;
    let hold = payer.allowance(&key).await.unwrap().hold;
    assert_eq!(
        refused,
        paykit_sdk::Refusal::AllowanceConfirmationNeeded { hold }
    );
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_open_attempt_across_an_orderly_generation_change_still_accounts() {
    // An attempt opened before an orderly generation change is still open after it: its
    // claim, of the earlier generation, still accounts for its reservation, and the
    // Allowance goes on paying (Pubky Dispatch spec, Sections 4.3 and 12.5).
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    world.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let open = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "5")).await;
    assert!(open.outcome.is_ok(), "{open:?}");
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    assert!(
        Box::pin(payer.reconcile_allowance(&key))
            .await
            .unwrap()
            .is_some(),
        "the open attempt of the earlier generation accounts"
    );
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_executor_compacted_ledger_across_an_orderly_generation_change_still_pays() {
    paykit_sdk::with_early_ledger_compaction(Box::pin(compacted_across_a_change())).await;
}

/// A failed attempt compacted into the ledger's totals before an orderly generation
/// change: the ledger written at the new generation still reads, payment after payment.
async fn compacted_across_a_change() {
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    world.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "7")).await;
    assert!(
        matches!(failed.outcome, Ok(paykit_sdk::PayOutcome::Failed { .. })),
        "{failed:?}"
    );
    world.clock.advance_ms(60_001);
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
    assert_eq!(only_ledger(&payer).await.0.failed.len(), 1, "compacted");
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    for value in ["2", "3"] {
        let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, value)).await;
        assert!(
            matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
            "{value}: {paid:?}"
        );
    }
}

#[tokio::test]
async fn test_executor_failed_attempt_across_an_orderly_generation_change_still_accounts() {
    // An attempt that failed before an orderly generation change: its outcome, of the
    // earlier generation, still accounts, and the Allowance goes on paying.
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    world.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "5")).await;
    assert!(
        matches!(failed.outcome, Ok(paykit_sdk::PayOutcome::Failed { .. })),
        "{failed:?}"
    );
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    assert!(Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_some());
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_executor_lost_generation_key_still_listed_pays_nothing() {
    // The next `identity.json` after a provider loss still lists the lost generation's key:
    // its writes verify, but nothing from before the loss counts, so the Allowance granted
    // then pays nothing (Paykit spec 8.3, "After provider loss").
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let _service = crate::provider_loss::declare_loss_keeping_key(&world);
    let uncertain_until = Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    world.clock.set_ms(
        uncertain_until.max(world.clock.now_ms()) + pubky_dispatch_client::app::DOC_CACHE_MS,
    );
    // The grant from before the loss pays nothing: the user confirms the Allowance again
    // under the loss's hold, and its usage waits for repair (Paykit spec 8.3, "After
    // provider loss").
    // The earlier grant's evidence verifies under the still-listed key, but is from before
    // the loss: reconciliation establishes nothing from it.
    assert!(granted(&payer, &key).await);
    assert!(payer.reconcile_allowance(&key).await.unwrap().is_none());
    let refused = Box::pin(subscription_refused(&world, &mut shop, &mut payer, &key)).await;
    let hold = payer.allowance(&key).await.unwrap().hold;
    assert_eq!(
        refused,
        paykit_sdk::Refusal::AllowanceConfirmationNeeded { hold }
    );
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_attempt_answered_by_the_earlier_generation_is_not_acted_on() {
    // After an orderly generation change, a reservation answered by the earlier
    // generation's service authorizes nothing: the answer, under the earlier key, isn't
    // taken, and no money moves (Pubky Dispatch spec, Section 12.5).
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let earlier = std::sync::Arc::clone(world.alice.switch());
    let service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world
        .router
        .add_switch(crate::common::ALICE_ORIGIN, &earlier);
    let answered = Box::pin(payer.pay_under_allowance(&request, &key)).await;
    assert!(
        !matches!(answered.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{answered:?}"
    );
    assert!(world.rail.all_executions().is_empty());
    world
        .router
        .add_switch(crate::common::ALICE_ORIGIN, &service);
}

#[tokio::test]
async fn test_executor_reservation_whose_answer_was_lost_counts_across_a_change() {
    // A payment's reservation applies at the service and its answer is lost, so the app's
    // checkpoint lacks it; the service's generation then changes in an orderly way. Later
    // reads count that reservation from its opening, as evidence of what happened, and the
    // Allowance goes on paying (Pubky Dispatch spec, Section 12.5).
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let lost = Box::pin(payer.pay_under_allowance(&request, &key)).await;
    assert!(lost.outcome.is_err(), "{lost:?}");
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let _ = Box::pin(payer.resume_payments()).await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(Box::pin(payer.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_some());
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "2")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

/// Restore the payer's `backup` over `storage`, list and reconcile, and confirm the
/// Allowance at `key` again, as a restore that isn't clean asks.
async fn restored_across_a_change(
    world: &World,
    backup: &paykit_sdk::PaykitBackup,
    storage: std::sync::Arc<dyn pubky_dispatch_client::store::StateStore>,
    key: &[u8; 32],
) -> paykit_sdk::Paykit {
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, storage),
        Default::default(),
        backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    let hold = restored.allowance(key).await.unwrap().hold;
    Box::pin(restored.confirm_allowance_executor(&hold, UserConfirmed::new()))
        .await
        .unwrap();
    assert!(restored.allowance(key).await.unwrap().consented);
    restored
}

#[tokio::test]
async fn test_executor_restore_after_an_orderly_generation_change_still_pays() {
    // A backup taken after an orderly generation change, restored on a new device: the
    // grant and ledger the earlier generation wrote are read back with no copy held here,
    // and still count as evidence of their own generation (Paykit spec 8.3; Pubky Dispatch
    // spec, Section 12.5).
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = Box::pin(restored_across_a_change(
        &world,
        &backup,
        crate::common::store(),
        &key,
    ))
    .await;
    let paid = Box::pin(pays_under(&world, &mut shop, &mut restored, &key, "4")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_executor_in_place_restore_across_an_orderly_generation_change_still_pays() {
    // A backup taken before an orderly generation change, restored in place after two
    // payments in the new generation: the installation's newer checkpoint pins the ledger
    // write after the backup's, across the change (Paykit spec 8.3; Pubky Dispatch spec,
    // Section 12.5).
    let storage = crate::common::store();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(storage.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let backup = payer.backup().await.unwrap();
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    for value in ["1", "2"] {
        let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, value)).await;
        assert!(
            matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
            "{paid:?}"
        );
    }
    drop(payer);
    // The executor's own reads show it: its history is selected again and the user's
    // confirmation lands.
    let mut restored = Box::pin(restored_across_a_change(&world, &backup, storage, &key)).await;
    assert!(Box::pin(restored.reconcile_allowance(&key))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_executor_started_payment_repeats_after_a_conflict_across_a_change() {
    // A payment started in the new generation under a grant of the earlier one, then the
    // Allowance conflicted, written in the new generation: the record moving to a later
    // generation and sequence is no stale instance, so the started payment repeats as it
    // does without a change (Paykit spec 8.3, "Conflicted"; Pubky Dispatch spec, Section 12.5).
    use paykit_sdk::rail::Behaviour;
    use paykit_sdk::PayOutcome;
    let store = crate::common::store();
    let (world, mut payer, mut shop, key) = Box::pin(first_grant_in(store.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let request = Box::pin(request_from(
        &mut shop,
        world.alice_id(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    world.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome
            .unwrap()
    else {
        panic!("the Allowance payment was not pending")
    };
    let held = payer.app().shared_record(&key).await.unwrap().unwrap();
    let record = paykit_lib::records::AllowanceRecord::read(
        held.content.value().unwrap(),
        &payer.identity(),
    )
    .unwrap();
    let rejection = world.sign_as(
        &world.bob,
        "shop",
        &world.alice,
        "paykit.allowance_rejection",
        vec![record.proposal.hash()],
        json!({"allowance_id": record.allowance_id.as_str()})
            .as_object()
            .cloned()
            .unwrap(),
    );
    let keys = world.bob.identity().app_keys("shop");
    let cert = world.bob.switch().app(&keys.app_id_bytes()).cert.unwrap();
    service
        .send_plaintext(
            world.bob.identity().public(),
            &cert,
            &rejection,
            &[payer.app_id()],
        )
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        payer.allowance(&key).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    let app_id = payer.app_id();
    drop(payer);
    let mut payer =
        Box::pin(world.load_on_clock(&world.alice, store, app_id, world.clock.clone())).await;
    repeat_without_method_history(&world);
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt_id), 2);
    assert_eq!(world.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_executor_first_grant_kept_across_an_orderly_generation_change_lands() {
    // The first grant's batch is kept unapplied over an orderly generation change and sent
    // again to the new generation's service: the grant it carries was confirmed in the
    // earlier generation, which is in its history, so it is accepted and pays (Paykit spec
    // 8.3, "After provider loss" pauses only after a loss).
    use pubky_dispatch_client::testing::Fault;
    let (world, mut payer, mut shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    world.rail.set_history_payer(payer.identity());
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is unanswered");
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    for _ in 0..3 {
        if Box::pin(payer.resume_allowance_executor(&key))
            .await
            .is_ok()
        {
            break;
        }
        Box::pin(crate::common::finish_listing(&mut payer)).await;
    }
    assert!(payer.allowance(&key).await.unwrap().consented);
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_executor_acceptance_applied_before_an_orderly_generation_change_is_taken_up() {
    // The acceptance's batch applies and its answer is lost before an orderly generation
    // change: read back after the change, it is evidence of its own generation, and the
    // executor takes the Allowance up (Paykit spec 8.3; Pubky Dispatch spec, Section 12.5).
    use pubky_dispatch_client::testing::Fault;
    let world = World::new();
    let mut payer = Box::pin(world.join_with(
        &world.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        Default::default(),
        crate::common::store(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    world.rail.set_history_payer(payer.identity());
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    Box::pin(payer.propose_executable_allowance(shop.identity(), terms(), UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let _ = Box::pin(payer.receive(None)).await;
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let key = payer.allowances().await.unwrap().pop().unwrap().record;
    assert_eq!(
        payer.allowance(&key).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_executor_acceptance_kept_across_an_orderly_generation_change_is_taken_up() {
    // The acceptance's batch is kept unapplied over an orderly generation change, and the
    // client signs it again for the new generation: it is still the acceptance the user
    // approved, and the executor takes the Allowance up (Paykit spec 8.3; Pubky Dispatch spec,
    // Section 12.5).
    use pubky_dispatch_client::testing::Fault;
    let world = World::new();
    let mut payer = Box::pin(world.join_with(
        &world.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        Default::default(),
        crate::common::store(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    world.rail.set_history_payer(payer.identity());
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    Box::pin(payer.propose_executable_allowance(shop.identity(), terms(), UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    let _ = Box::pin(payer.receive(None)).await;
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let key = payer.allowances().await.unwrap().pop().unwrap().record;
    assert_eq!(
        payer.allowance(&key).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let paid = Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

/// An orderly change of `payer`'s generation whose new `identity.json` doesn't list the
/// earlier generation's service key.
async fn change_dropping_the_earlier_key(
    w: &World,
    payer: &mut paykit_sdk::Paykit,
) -> std::sync::Arc<pubky_dispatch_client::testing::ScriptedSwitch> {
    use crate::common::ALICE_ORIGIN;
    use pubky_dispatch_core::records::Inbox;
    let service = w.alice.switch().change_key_into([75; 32]);
    w.router.add_switch(ALICE_ORIGIN, &service);
    let doc = w
        .alice
        .sign_next(|body| {
            body.generation += 1;
            body.inbox = Inbox::new(ALICE_ORIGIN, service.service_key());
        })
        .unwrap();
    service.deliver_identity(&doc).unwrap();
    payer.app_mut().refresh_identity().await.unwrap();
    Box::pin(crate::common::finish_listing(payer)).await;
    service
}

/// `payer`'s `backup` restored on a new device, listed and reconciled.
async fn restored_on_a_new_device(
    world: &World,
    backup: &paykit_sdk::PaykitBackup,
) -> paykit_sdk::Paykit {
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, crate::common::store()),
        Default::default(),
        backup,
        world.rail.clone(),
        world.timer(),
    ))
    .await
    .unwrap();
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    Box::pin(finish_restored_reconciliation(&mut restored)).await;
    restored
}

#[tokio::test]
async fn test_executor_restore_after_a_change_whose_key_is_no_longer_listed_pays_nothing() {
    // The earlier generation's key is gone from `identity.json`: the app's own retained
    // proof of its grant verifies with no listed key, so after a restore it vouches for
    // nothing, and the Allowance pays nothing (Pubky Dispatch spec, Section 12.5).
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let _service = Box::pin(change_dropping_the_earlier_key(&world, &mut payer)).await;
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = Box::pin(restored_on_a_new_device(&world, &backup)).await;
    let hold = restored.allowance(&key).await.unwrap().hold;
    assert_eq!(
        Box::pin(restored.confirm_allowance_executor(&hold, UserConfirmed::new()))
            .await
            .unwrap_err()
            .refusal(),
        Some(&paykit_sdk::Refusal::HeldForUser(
            paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
                "the executor cannot establish the original Allowance grant and ledger"
            )
        ))
    );
    assert_eq!(
        Box::pin(subscription_refused(&world, &mut shop, &mut restored, &key)).await,
        paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
            "the accepted executor grant, ledger or wallet history is unavailable"
        ))
    );
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_restore_after_a_provider_loss_pays_nothing_under_the_earlier_grant() {
    // After a provider loss whose `identity.json` still lists the lost generation's key, a
    // restored app's own retained proof of its earlier grant verifies, but its generation
    // is before the loss: it vouches for nothing (Paykit spec 8.3, "After provider loss").
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.rail.set_history_payer(payer.identity());
    let _service = crate::provider_loss::declare_loss_keeping_key(&world);
    let uncertain_until = Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    world.clock.set_ms(
        uncertain_until.max(world.clock.now_ms()) + pubky_dispatch_client::app::DOC_CACHE_MS,
    );
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = Box::pin(restored_on_a_new_device(&world, &backup)).await;
    restored.app_mut().invite(&shop.identity()).await.unwrap();
    // The earlier grant's evidence verifies under the still-listed key, but is from before
    // the loss: reconciliation establishes nothing from it.
    assert!(granted(&restored, &key).await);
    assert!(restored.reconcile_allowance(&key).await.unwrap().is_none());
    // Nothing written: the record isn't read as the grant's write needs yet.
    let hold = restored.allowance(&key).await.unwrap().hold;
    let before = Box::pin(regrant::confirmed(&restored, &key)).await;
    let refused = Box::pin(restored.confirm_allowance_executor(&hold, UserConfirmed::new()))
        .await
        .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&paykit_sdk::Refusal::HeldForUser(
            paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
                "the Allowance record isn't read as writing on it needs yet"
            )
        ))
    );
    assert_eq!(Box::pin(regrant::confirmed(&restored, &key)).await, before);
    // Paid nothing: the user confirms the Allowance again under the loss's hold first.
    let refused = Box::pin(subscription_refused(&world, &mut shop, &mut restored, &key)).await;
    let hold = restored.allowance(&key).await.unwrap().hold;
    assert_eq!(
        refused,
        paykit_sdk::Refusal::AllowanceConfirmationNeeded { hold }
    );
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_restore_of_a_backup_from_before_the_grant_across_a_change() {
    // The backup was taken before the grant's write: the restored app retains no operation
    // of its own to vouch for the grant read back after the change.
    let (world, mut payer, mut shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    world.rail.set_history_payer(payer.identity());
    let backup = payer.backup().await.unwrap();
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    drop(payer);
    let mut restored = Box::pin(restored_on_a_new_device(&world, &backup)).await;
    // Nothing establishes the grant here: confirming and resuming are held, and nothing is
    // paid. The user can still end the Allowance.
    assert!(!restored.allowance(&key).await.unwrap().consented);
    let hold = restored.allowance(&key).await.unwrap().hold;
    let expected =
        paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
            "this app's grant is held, but its consent isn't on this device",
        ));
    for refused in [
        Box::pin(restored.confirm_allowance_executor(&hold, UserConfirmed::new())).await,
        Box::pin(restored.resume_allowance_executor(&key)).await,
    ] {
        assert_eq!(refused.unwrap_err().refusal(), Some(&expected));
    }
    let paid = Box::pin(pays_under(&world, &mut shop, &mut restored, &key, "1")).await;
    assert_eq!(paid.outcome.unwrap_err().refusal(), Some(&expected));
    assert!(world.rail.all_executions().is_empty());
    Box::pin(restored.end_allowance(&key)).await.unwrap();
    assert_eq!(
        restored.allowance(&key).await.unwrap().status,
        AllowanceStatus::Ended
    );
}

#[tokio::test]
async fn test_executor_first_grant_after_a_change_and_a_restore_lands() {
    // The acceptance applied in the earlier generation; the backup holds it, and after the
    // change the restored app reads the pending proposal's deletion with no copy held: its
    // own retained deletion vouches for it, and the first grant lands (Paykit wire 5.9).
    let (world, mut payer, mut shop, key) =
        Box::pin(accepted_not_granted(crate::common::store())).await;
    world.rail.set_history_payer(payer.identity());
    let _service = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut restored = Box::pin(restored_on_a_new_device(&world, &backup)).await;
    let hold = restored.allowance(&key).await.unwrap().hold;
    Box::pin(restored.confirm_allowance_executor(&hold, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(pays_under(&world, &mut shop, &mut restored, &key, "1")).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_executor_older_in_place_restore_keeps_a_kept_end_and_its_removal() {
    // The executor is backed up with its Allowance granted. The shop then ends it: the
    // executor reads the end, keeps it as the stop and removes the Allowance from its
    // consent list, but the end's placement in the record doesn't reach the Dispatch, and the
    // read is acknowledged, so the inbox no longer holds the end. Restored in place from the
    // older backup, with the shop sending nothing again, the installation's stop and
    // removal stay: no payment goes under the ended Allowance (Paykit spec 8.3).
    use pubky_dispatch_client::testing::Fault;
    let storage = crate::common::store();
    let (world, mut payer, mut shop, key) =
        Box::pin(first_grant_in(storage.clone(), terms())).await;
    world.rail.set_history_payer(payer.identity());
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("5", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let backup = payer.backup().await.unwrap();
    let accepted = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.end_allowance(&accepted.record))
        .await
        .unwrap();
    for _ in 0..4 {
        world.alice.switch().fail_next(Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    }
    let _ = Box::pin(payer.receive(None)).await;
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&payer, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    // The read is acknowledged once the Dispatch answers again.
    let mut flushed = payer.app_mut().flush_cursor().await;
    for _ in 0..4 {
        if flushed.is_ok() {
            break;
        }
        flushed = payer.app_mut().flush_cursor().await;
    }
    flushed.unwrap();
    drop(payer);
    let mut restored = Box::pin(paykit_sdk::Paykit::restore(
        world.env(&world.alice, storage),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&world.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        world.timer(),
    ))
    .await
    .unwrap();
    // Before any read: the installation's kept end and removal stand, with no mark of a
    // consent this restore holds.
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&restored, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    assert!(restored
        .app()
        .protocol_scan("paykit", b"rsc/")
        .await
        .unwrap()
        .is_empty());
    assert!(restored
        .app()
        .protocol_scan("paykit", b"xr/")
        .await
        .unwrap()
        .is_empty());
    Box::pin(crate::common::finish_listing(&mut restored)).await;
    let mut reconciled = Box::pin(restored.reconcile()).await;
    for _ in 0..8 {
        let Err(error) = &reconciled else { break };
        assert!(
            matches!(error.refusal(), Some(paykit_sdk::Refusal::NotNow(_))),
            "{error}"
        );
        reconciled = Box::pin(restored.reconcile()).await;
    }
    reconciled.unwrap();
    Box::pin(restored.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    // No approval for execution of the removed Allowance came back with the restore.
    assert!(restored
        .app()
        .protocol_scan("paykit", b"xa/")
        .await
        .unwrap()
        .is_empty());
    // The end is the stop the installation kept, and the ordinary passes place it in the
    // Allowance record: the record shows it ended, with no stop left to place.
    assert_eq!(
        restored.allowance(&key).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(restored
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    // The removal is the installation's: no confirmation of the user's brings the
    // Allowance back.
    assert_eq!(
        Box::pin(crate::allowances::consent_entry(&restored, &key)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    let hold = restored.allowance(&key).await.unwrap().hold;
    let confirmed =
        Box::pin(restored.confirm_allowance_executor(&hold, UserConfirmed::new())).await;
    assert!(confirmed.is_err(), "{confirmed:?}");
    let paid = Box::pin(restored.pay_under_allowance(&request, &key))
        .await
        .outcome;
    assert!(
        !matches!(paid, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "paid under an ended Allowance: {paid:?}"
    );
    assert!(world.rail.all_executions().is_empty());
    // The ended Allowance stays ended. A new one, proposed to the shop and accepted, is
    // granted and on the consent list.
    let proposal = Box::pin(restored.propose_executable_allowance(
        shop.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.allowance_id == proposal.allowance_id)
        .unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    Box::pin(restored.receive(None)).await.unwrap();
    Box::pin(restored.resume_allowance_executor(&proposal.record))
        .await
        .unwrap();
    assert!(
        restored
            .allowance(&proposal.record)
            .await
            .unwrap()
            .consented
    );
}

#[tokio::test]
async fn test_executor_pays_nothing_under_the_shared_post_loss_hold() {
    // Paykit spec 8.1; Paykit wire 5.6, Shared post-loss hold: after a declared provider loss
    // the executor pays no one-time request under the Allowance, one created long after the
    // loss included. A confirmation of the executor after the loss writes the grant again at
    // the current generation, and the shared hold still pays nothing.
    let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
    world.clock.advance_ms(60_000);
    crate::provider_loss::declare_loss(&world);
    let until = Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    // The user's confirmation after the loss writes the grant once more, at the current
    // generation; it establishes no usage authority, and the shared hold stays.
    let hold = payer.allowance(&key).await.unwrap().hold;
    let (_, grants) = Box::pin(regrant::confirmed(&payer, &key)).await;
    Box::pin(payer.confirm_allowance_executor(&hold, UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let generation = payer.app().own_identity().generation();
    assert_eq!(
        Box::pin(regrant::confirmed(&payer, &key)).await,
        (generation, grants + 1)
    );
    let now = world.clock.now_ms();
    world
        .clock
        .advance_ms(until.saturating_sub(now) + 3_600_000);
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", world.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let refused = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::NotAutomatic(why)) if why.contains("provider loss")
        ),
        "{refused}"
    );
    assert!(world.rail.all_pay_calls().is_empty());
}

/// An orderly change of `payer`'s generation whose new `identity.json` lists the earlier
/// generations' keys except those of `dropped`, as the signer may once no current record or
/// live claim needs them (Pubky Dispatch spec, Section 4.3, `service_keys`).
async fn change_dropping(
    w: &World,
    from: &pubky_dispatch_client::testing::ScriptedSwitch,
    payer: &mut paykit_sdk::Paykit,
    dropped: &[u64],
    secret: [u8; 32],
) -> std::sync::Arc<pubky_dispatch_client::testing::ScriptedSwitch> {
    use crate::common::ALICE_ORIGIN;
    use pubky_dispatch_core::records::{Inbox, ServiceKeyEntry};
    let previous = payer.app().own_identity().clone();
    let service = from.change_key_into(secret);
    w.router.add_switch(ALICE_ORIGIN, &service);
    let doc = w
        .alice
        .sign_next(|body| {
            body.service_keys.push(ServiceKeyEntry {
                generation: previous.generation(),
                key: previous.inbox().service_key,
            });
            body.service_keys
                .retain(|entry| !dropped.contains(&entry.generation));
            body.generation += 1;
            body.inbox = Inbox::new(ALICE_ORIGIN, service.service_key());
        })
        .unwrap();
    service.deliver_identity(&doc).unwrap();
    payer.app_mut().refresh_identity().await.unwrap();
    Box::pin(crate::common::finish_listing(payer)).await;
    for generation in dropped {
        assert!(payer
            .app()
            .own_identity()
            .service_key_of(*generation)
            .is_none());
    }
    service
}

#[tokio::test]
async fn test_executor_pays_after_the_proposals_generation_key_is_dropped() {
    Box::pin(proposal_across_a_move(true)).await;
}

#[tokio::test]
async fn test_executor_requires_proposal_key_without_checkpoint() {
    Box::pin(proposal_across_a_move(false)).await;
}

async fn proposal_across_a_move(establish: bool) {
    // Acceptance supersedes the proposal in the next generation. Only its original
    // placement needs the key dropped in the following move (Dispatch spec 4.3).
    let world = World::new();
    let mut payer = Box::pin(world.join_with(
        &world.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        Default::default(),
        crate::common::store(),
    ))
    .await;
    let mut shop = Box::pin(world.join(
        &world.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    world.rail.set_history_payer(payer.identity());
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    let first = payer.app().own_identity().generation();
    Box::pin(payer.propose_executable_allowance(shop.identity(), terms(), UserConfirmed::new()))
        .await
        .unwrap();
    let second = Box::pin(crate::allowances::advance_allowance_generation(
        &world, &mut payer,
    ))
    .await;
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let key = payer.allowances().await.unwrap().pop().unwrap().record;
    if establish {
        Box::pin(payer.resume_allowance_executor(&key))
            .await
            .unwrap();
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
    }
    assert_eq!(granted(&payer, &key).await, establish);
    let _third = Box::pin(change_dropping(
        &world,
        &second,
        &mut payer,
        &[first],
        [76; 32],
    ))
    .await;
    if establish {
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "2")).await;
    } else {
        let hold = payer.allowance(&key).await.unwrap().hold;
        let refused = Box::pin(payer.confirm_allowance_executor(&hold, UserConfirmed::new()))
            .await
            .unwrap_err();
        assert_eq!(
            refused.refusal(),
            Some(&paykit_sdk::Refusal::HeldForUser(
                paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
                    "the executor cannot establish the original Allowance grant and ledger"
                )
            ))
        );
        assert!(!granted(&payer, &key).await);
        assert!(
            Box::pin(pays_under(&world, &mut shop, &mut payer, &key, "2"))
                .await
                .outcome
                .is_err()
        );
        assert!(world.rail.all_pay_calls().is_empty());
    }
}

async fn assert_pays_under(
    world: &World,
    shop: &mut paykit_sdk::Paykit,
    payer: &mut paykit_sdk::Paykit,
    key: &[u8; 32],
    value: &str,
) {
    let paid = Box::pin(pays_under(world, shop, payer, key, value)).await;
    assert!(
        matches!(paid.outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{paid:?}"
    );
}

async fn assert_compaction_receipt(
    payer: &paykit_sdk::Paykit,
    before: u64,
    after: u64,
    opening: bool,
) {
    let rows = payer.app().protocol_scan("paykit", b"lx/").await.unwrap();
    assert_eq!(rows.len(), 1);
    let row: serde_json::Value = serde_json::from_slice(&rows[0].1).unwrap();
    let receipt = &row["d"];
    let generation = |field: &str| {
        let kept: pubky_dispatch_client::app::KeptOp =
            serde_json::from_value(receipt[field].clone()).unwrap();
        TransitionProof::peek(&kept.proof.unwrap())
            .unwrap()
            .envelope()
            .generation
    };
    assert_eq!((generation("before"), generation("after")), (before, after));
    assert_eq!(!receipt.get("opening").unwrap().is_null(), opening);
    if opening {
        assert_eq!(generation("opening"), after);
    }
}

#[tokio::test]
async fn test_executor_pays_after_compacting_reservation_key_retirement() {
    paykit_sdk::with_early_ledger_compaction(Box::pin(async {
        let (world, mut payer, mut shop, key) = Box::pin(first_grant()).await;
        world.rail.set_history_payer(payer.identity());
        let second = Box::pin(crate::allowances::advance_allowance_generation(
            &world, &mut payer,
        ))
        .await;
        let generation = second.generation();
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "7")).await;
        world.clock.advance_ms(60_001);
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "3")).await;
        assert_compaction_receipt(&payer, generation, generation, true).await;
        let (ledger, _) = only_ledger(&payer).await;
        assert_eq!(ledger.count, 1);
        let claim = payer
            .app_mut()
            .read_claim(&ledger.entries[0].obligation)
            .await
            .unwrap()
            .unwrap();
        assert!(claim.view.tombstone && claim.entries.is_empty());

        // Supersede the ledger and both payer records before retiring their key (Dispatch spec 4.3).
        let third = Box::pin(change_dropping(&world, &second, &mut payer, &[], [75; 32])).await;
        Box::pin(crate::common::finish_listing(&mut shop)).await;
        let sales = shop.payee_activity().await.unwrap();
        assert_eq!(sales.len(), 2);
        for sale in sales {
            Box::pin(shop.confirm_payment(&sale.record))
                .await
                .unwrap()
                .unwrap();
        }
        Box::pin(crate::common::finish_listing(&mut payer)).await;
        let activities = payer.app().records_of("payer_activity").await.unwrap();
        assert_eq!(activities.len(), 2);
        assert!(activities
            .iter()
            .all(|r| r.version.generation == third.generation()));
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "2")).await;
        assert_eq!(only_ledger(&payer).await.1.generation, third.generation());
        let _moved = Box::pin(change_dropping(
            &world,
            &third,
            &mut payer,
            &[generation],
            [76; 32],
        ))
        .await;
        Box::pin(assert_pays_under(&world, &mut shop, &mut payer, &key, "1")).await;
        assert_eq!(
            Box::pin(payer.reconcile_allowance(&key))
                .await
                .unwrap()
                .unwrap()
                .lifetime()
                .as_str(),
            "13"
        );
    }))
    .await;
}
