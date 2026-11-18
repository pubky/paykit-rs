//! Readers of an issued one-time payee receipt over a shared deletion (Paykit wire 5).
use crate::common::{draft, request_from, store, World};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ActivityRecord, ActivitySide};
use paykit_sdk::{AppScopes, PayOptions, Paykit, Refusal};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
use pubky_switch_client::clock::Clock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadCase {
    Live,
    Reload,
    FreshRestore,
    Complete,
    MissingNote,
    ConflictingNote,
    MissingDeleteCondition,
    MissingCondition,
    Recovered,
}

#[tokio::test]
async fn test_receipt_reader_accepts_the_proved_repair() {
    Box::pin(read_repaired_marker(ReadCase::Live)).await;
}

#[tokio::test]
async fn test_receipt_reader_reloads_the_held_marked_receipt() {
    Box::pin(read_repaired_marker(ReadCase::Reload)).await;
}

#[tokio::test]
async fn test_receipt_reader_restores_the_held_marked_receipt() {
    Box::pin(read_repaired_marker(ReadCase::FreshRestore)).await;
}

#[tokio::test]
async fn test_receipt_reader_retains_a_missing_nested_member() {
    Box::pin(read_repaired_marker(ReadCase::MissingNote)).await;
}

#[tokio::test]
async fn test_receipt_reader_retains_a_conflicting_nested_member() {
    Box::pin(read_repaired_marker(ReadCase::ConflictingNote)).await;
}

#[tokio::test]
async fn test_receipt_reader_accepts_a_complete_successor_to_the_deletion() {
    Box::pin(read_repaired_marker(ReadCase::Complete)).await;
}

#[tokio::test]
async fn test_receipt_reader_requires_the_exact_deletion_application() {
    Box::pin(read_repaired_marker(ReadCase::MissingDeleteCondition)).await;
}

#[tokio::test]
async fn test_receipt_reader_requires_the_exact_marked_application() {
    Box::pin(read_repaired_marker(ReadCase::MissingCondition)).await;
}

#[tokio::test]
async fn test_receipt_reader_preserves_existing_credit_evidence_after_provider_loss() {
    Box::pin(read_repaired_marker(ReadCase::Recovered)).await;
}

async fn read_repaired_marker(case: ReadCase) {
    let w = World::new();
    let scopes = || AppScopes::new().request().build().unwrap();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w.join(&w.bob, "till", scopes()).await;
    let device = store();
    let mut witness = w
        .join_with(
            &w.bob,
            "witness",
            scopes(),
            Default::default(),
            device.clone(),
        )
        .await;
    let mut late = w.join(&w.bob, "late", scopes()).await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.payee_activity().await.unwrap()[0].record;
    let msg_id = shop.confirm_payment(&key).await.unwrap().unwrap();
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let issued = shop.app().shared_record(&key).await.unwrap().unwrap();
    let signed = ActivityRecord::read(
        issued.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap()
    .receipt
    .unwrap();
    assert_eq!(signed.message().msg_id, msg_id);
    let mut original: serde_json::Value =
        serde_json::from_slice(issued.content.value().unwrap()).unwrap();
    original["proof"]["cert"]["merchant_note"] = serde_json::json!("collected");
    shop.app_mut()
        .put_record(&target, &serde_json::to_vec(&original).unwrap(), Vec::new())
        .await
        .unwrap();
    for reader in [&mut witness, &mut till] {
        reader.receive(None).await.unwrap();
        let held = reader.app().shared_record(&key).await.unwrap().unwrap();
        assert!(held.authenticated && !held.flagged);
        let held: serde_json::Value =
            serde_json::from_slice(held.content.value().unwrap()).unwrap();
        assert_eq!(held, original);
    }
    let deletion = till
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    if case == ReadCase::Recovered {
        Box::pin(keep_recovered_delete(
            &w, &mut shop, &mut till, &target, &signed,
        ))
        .await;
        return;
    }
    let writes_at_delete = w.record_writes(&w.bob, &key);
    witness.receive(None).await.unwrap();
    witness.app_mut().flush_cursor().await.unwrap();
    assert!(w.bob.switch().app(&witness.app_id()).cursor < deletion.version.version);
    assert_eq!(w.record_writes(&w.bob, &key), writes_at_delete);
    let notice = witness
        .payee_deletion_notices()
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(notice.writer, Some(till.app_id()));
    if case == ReadCase::Complete {
        let published = till
            .app_mut()
            .put_record(&target, &serde_json::to_vec(&original).unwrap(), Vec::new())
            .await
            .unwrap();
        witness.app_mut().read_record_fresh(&target).await.unwrap();
        witness.receive(None).await.unwrap();
        witness.app_mut().flush_cursor().await.unwrap();
        let read = witness.app().shared_record(&key).await.unwrap().unwrap();
        assert!(read.authenticated && !read.flagged && !read.not_taken);
        assert_eq!(read.version, published.version);
        assert_eq!(read.content, published.content);
        assert!(w.bob.switch().app(&witness.app_id()).cursor >= deletion.version.version);
        assert_eq!(
            witness.retained_payee_receipts().await.unwrap()[0]
                .receipt
                .hash(),
            signed.hash()
        );
        assert!(matches!(
            witness.confirm_payment(&key).await.unwrap_err().refusal(),
            Some(Refusal::CreditHistoryLost)
        ));
        assert_eq!(w.record_writes(&w.bob, &key), writes_at_delete + 1);
        assert_eq!(w.rail.all_executions().values().copied().sum::<u64>(), 1);
        return;
    }
    let mut eager = original.clone();
    eager["lost_to_delete"] = pubky_switch_core::enc::b64(&notice.delete).into();
    let cert = eager["proof"]["cert"].as_object_mut().unwrap();
    cert.remove("merchant_note");
    cert.insert("shipping_note".into(), "delivered".into());
    let condition = pubky_switch_core::ops::Condition::RecordCurrent {
        key,
        revision: pubky_switch_core::shared::ApplicationRef {
            generation: deletion.version.generation,
            seq: deletion.version.version,
            item_hash: notice.delete,
        },
        deleted: true,
    };
    let conditions = if case == ReadCase::MissingDeleteCondition {
        Vec::new()
    } else {
        vec![condition]
    };
    let eager_write = till
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&eager).unwrap(), conditions)
        .await
        .unwrap();
    let before = w.record_writes(&w.bob, &key);
    witness.app_mut().read_record_fresh(&target).await.unwrap();
    late.app_mut().read_record_fresh(&target).await.unwrap();
    witness.app_mut().flush_cursor().await.unwrap();
    assert!(w.bob.switch().app(&witness.app_id()).cursor < deletion.version.version);
    for reader in [&witness, &late] {
        let held = reader.app().shared_record(&key).await.unwrap().unwrap();
        assert!(held.authenticated && !held.flagged);
        let value: serde_json::Value =
            serde_json::from_slice(held.content.value().unwrap()).unwrap();
        assert_eq!(value, eager);
    }
    assert_eq!(w.record_writes(&w.bob, &key), before);
    assert!(late.retained_payee_receipts().await.unwrap().is_empty());
    let app_id = witness.app_id();
    let backup = witness.backup().await.unwrap();
    shop.app_mut().read_record_fresh(&target).await.unwrap();
    let mut union = original;
    union["proof"]["cert"]["shipping_note"] = "delivered".into();
    if case == ReadCase::MissingNote {
        union["proof"]["cert"]
            .as_object_mut()
            .unwrap()
            .remove("shipping_note");
    } else if case == ReadCase::ConflictingNote {
        union["proof"]["cert"]["shipping_note"] = "returned".into();
    }
    let eager_hash = w
        .bob
        .switch()
        .applied()
        .into_iter()
        .rev()
        .find(|op| {
            matches!(
                &op.body().kind, pubky_switch_core::ops::OpKind::RecordPut(put) if put.key == key
            )
        })
        .unwrap()
        .item_hash()
        .unwrap();
    let conditions = if case == ReadCase::MissingCondition {
        Vec::new()
    } else {
        vec![pubky_switch_core::ops::Condition::RecordCurrent {
            key,
            revision: pubky_switch_core::shared::ApplicationRef {
                generation: eager_write.version.generation,
                seq: eager_write.version.version,
                item_hash: eager_hash,
            },
            deleted: false,
        }]
    };
    let repaired = shop
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&union).unwrap(), conditions)
        .await
        .unwrap();
    if matches!(case, ReadCase::Reload | ReadCase::FreshRestore) {
        drop(witness);
        witness = if case == ReadCase::Reload {
            w.load_on_clock(&w.bob, device, app_id, w.clock.clone())
                .await
        } else {
            Box::pin(Paykit::restore(
                w.env(&w.bob, store()),
                Default::default(),
                &backup,
                w.rail.clone(),
                w.timer(),
            ))
            .await
            .unwrap()
        };
    }
    witness.app_mut().read_record_fresh(&target).await.unwrap();
    witness.receive(None).await.unwrap();
    witness.app_mut().flush_cursor().await.unwrap();
    let read = witness.app().shared_record(&key).await.unwrap().unwrap();
    let covered = matches!(
        case,
        ReadCase::Live | ReadCase::Reload | ReadCase::FreshRestore
    );
    let cursor = w.bob.switch().app(&app_id).cursor;
    println!(
        "receipt reader {}",
        serde_json::json!({
            "case": format!("{case:?}"), "delete": deletion.version.version, "marked_write": eager_write.version.version,
            "publication": repaired.version.version, "service_cursor": cursor, "accepted": read.version.version,
            "receipt": pubky_switch_core::enc::b64(&signed.hash()), "flagged": read.flagged, "not_taken": read.not_taken,
        })
    );
    assert_eq!(cursor >= deletion.version.version, covered);
    assert_eq!(
        !read.flagged && !read.not_taken && read.authenticated,
        covered
    );
    if covered {
        assert_eq!(read.version, repaired.version);
        assert_eq!(read.writer, shop.app_id());
        assert_eq!(read.content, repaired.content);
    } else {
        assert_eq!(read.version, eager_write.version);
        assert_eq!(read.content, eager_write.content);
        let held = ActivityRecord::read(
            read.content.value().unwrap(),
            &w.bob_id(),
            ActivitySide::Payee,
        )
        .unwrap();
        assert_eq!(held.lost_to_delete, Some(notice.delete));
        assert_eq!(held.receipt.unwrap().hash(), signed.hash());
    }
    assert_eq!(w.record_writes(&w.bob, &key), before + 1);
    late.app_mut().read_record_fresh(&target).await.unwrap();
    let unproved = late.app().shared_record(&key).await.unwrap().unwrap();
    assert!(unproved.flagged);
    let retained = ActivityRecord::read(
        unproved.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    assert_eq!(retained.lost_to_delete, Some(notice.delete));
    assert_eq!(retained.receipt.unwrap().hash(), signed.hash());
    assert_eq!(
        witness.retained_payee_receipts().await.unwrap()[0]
            .receipt
            .hash(),
        signed.hash()
    );
    assert_eq!(w.record_writes(&w.bob, &key), before + 1);
    assert_eq!(w.rail.all_executions().values().copied().sum::<u64>(), 1);
    assert!(matches!(
        witness.confirm_payment(&key).await.unwrap_err().refusal(),
        Some(Refusal::CreditHistoryLost)
    ));
    assert_eq!(w.record_writes(&w.bob, &key), before + 1);
}

async fn keep_recovered_delete(
    w: &World,
    shop: &mut Paykit,
    till: &mut Paykit,
    target: &RecordTarget,
    receipt: &paykit_lib::records::StoredMessage,
) {
    let key = shop.app().record_key(target).unwrap();
    let paykit_lib::Body::PaymentReceipt(payment) = receipt.body() else {
        panic!("expected signed receipt");
    };
    let service = pubky_switch_client::testing::ScriptedSwitch::recovering(
        w.bob.identity().key(),
        [25; 32],
        w.clock.clone(),
        2,
        1,
    )
    .unwrap();
    let origin = "https://recovered-shop.example";
    w.router.add_switch(origin, &service);
    let doc = w
        .bob
        .sign_next(|body| {
            body.generation = 2;
            body.recovery_from = Some(1);
            body.inbox = pubky_switch_core::records::Inbox::new(origin, service.service_key());
        })
        .unwrap();
    service.deliver_identity(&doc).unwrap();
    till.app_mut().refresh_identity().await.unwrap();
    till.receive(None).await.unwrap();
    let deleted_op = w
        .bob
        .switch()
        .applied()
        .into_iter()
        .find(|op| {
            matches!(&op.body().kind, pubky_switch_core::ops::OpKind::RecordPut(put)
            if put.key == key && put.value_sealed.looks_like_delete())
        })
        .unwrap();
    assert!(service
        .uploads()
        .iter()
        .any(
            |(body, _)| body["records"].as_array().is_some_and(|records| records
                .iter()
                .any(|r| r["op"] == serde_json::to_value(deleted_op.signed()).unwrap()))
        ));
    let mut recovery_cursors = vec![service.app(&shop.app_id()).cursor];
    shop.app_mut().refresh_identity().await.unwrap();
    recovery_cursors.push(service.app(&shop.app_id()).cursor);
    shop.receive(None).await.unwrap();
    recovery_cursors.push(service.app(&shop.app_id()).cursor);
    let known_after_feed = !shop.retained_payee_receipts().await.unwrap().is_empty();
    shop.app_mut().read_record_fresh(target).await.unwrap();
    recovery_cursors.push(service.app(&shop.app_id()).cursor);
    shop.app_mut().flush_cursor().await.unwrap();
    assert_eq!(shop.app().cursor().generation, 2);
    let pubky_switch_core::ops::Condition::RecordCurrent {
        revision,
        deleted: true,
        ..
    } = shop.app().record_condition(&key).await.unwrap()
    else {
        panic!("the recovered application of the original deletion is current");
    };
    assert_eq!(revision.generation, 2);
    assert_eq!(revision.item_hash, deleted_op.item_hash().unwrap());
    recovery_cursors.push(service.app(&shop.app_id()).cursor);
    println!(
        "receipt after recovery {}",
        serde_json::json!({
            "record_upload": revision.seq,
            "receipt_known_after_feed": known_after_feed,
            "cursors_before_refresh_after_refresh_feed_view_flush": recovery_cursors,
        })
    );
    let retained = shop.retained_payee_receipts().await.unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].receipt.hash(), receipt.hash());
    assert!(retained[0]
        .deletions
        .iter()
        .any(|d| d.delete == deleted_op.item_hash().unwrap()));
    assert!(matches!(
        shop.confirm_payment(&key).await.unwrap_err().refusal(),
        Some(Refusal::CreditHistoryLost)
    ));
    assert!(service.applied().iter().all(|op|
        !matches!(&op.body().kind, pubky_switch_core::ops::OpKind::RecordPut(put) if put.key == key)));
    assert_eq!(w.rail.executions(&payment.attempt_id), 1);
}
