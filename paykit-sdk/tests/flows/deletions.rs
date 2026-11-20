//! An app retains a signed receipt when another own app deletes its activity (Paykit spec 6).

use crate::common::{draft, request_from, World};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_sdk::{AppScopes, PayOptions, Refusal};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
use pubky_switch_client::clock::Clock;

#[tokio::test]
async fn test_payee_deletion_observation_displays_the_original_receipt_and_preserves_the_hold() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.payee_activity().await.unwrap()[0].record;
    let signed = shop.confirm_payment(&key).await.unwrap().unwrap();
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    seller_wallet.receive(None).await.unwrap();
    let original = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    let deleted = seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    let writes = w.record_writes(&w.bob, &key);
    let at = w.clock.now_ms();
    shop.receive(None).await.unwrap();
    let notices = shop.payee_deletion_notices().await.unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].writer, Some(seller_wallet.app_id()));
    assert_eq!(notices[0].record, key);
    assert_eq!(notices[0].generation, deleted.version.generation);
    assert_eq!(notices[0].version, deleted.version.version);
    assert_eq!(notices[0].page_time_ms, Some(at));
    let held = shop.retained_payee_receipts().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].record, key);
    assert_eq!(held[0].deletions[0].version, deleted.version);
    assert_eq!(held[0].deletions[0].writer, Some(seller_wallet.app_id()));
    assert_eq!(held[0].deletions[0].page_time_ms, Some(at));
    assert_eq!(held[0].deletions[0].delete, notices[0].delete);
    assert_eq!(held[0].receipt.message().msg_id, signed);
    let prior = paykit_lib::records::ActivityRecord::read(
        original.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap()
    .receipt
    .unwrap();
    assert_eq!(held[0].receipt.hash(), prior.hash());
    assert!(matches!(
        shop.confirm_payment(&key).await.unwrap_err(),
        paykit_sdk::PaykitError::Refused(Refusal::CreditHistoryLost)
    ));
    let backup = shop.backup().await.unwrap();
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.bob, crate::common::store()),
        Default::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let from_backup = restored.payee_deletion_notices().await.unwrap();
    assert_eq!(from_backup.len(), 1);
    assert_eq!(from_backup[0].writer, None);
    assert_eq!(from_backup[0].page_time_ms, None);
    assert_eq!(from_backup[0].delete, notices[0].delete);
    restored
        .acknowledge_payee_deletion(&from_backup[0])
        .await
        .unwrap();
    w.clock.advance_ms(30_000);
    restored.receive(None).await.unwrap();
    assert!(restored.payee_deletion_notices().await.unwrap().is_empty());
    assert_eq!(
        restored.retained_payee_receipts().await.unwrap()[0]
            .receipt
            .hash(),
        prior.hash()
    );
    assert_eq!(w.record_writes(&w.bob, &key), writes + 1);
    let pubky_switch_client::app::RecordRead::Fresh(published) = seller_wallet
        .app_mut()
        .read_record_fresh(&target)
        .await
        .unwrap()
    else {
        panic!("the switch must return the published receipt");
    };
    let published = paykit_lib::records::ActivityRecord::read(
        published.content.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    assert_eq!(published.receipt.unwrap().hash(), prior.hash());
    seller_wallet
        .app_mut()
        .put_record(&target, original.value().unwrap(), Vec::new())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    assert!(matches!(
        restored.confirm_payment(&key).await.unwrap_err(),
        paykit_sdk::PaykitError::Refused(Refusal::CreditHistoryLost)
    ));
    assert_eq!(
        restored.retained_payee_receipts().await.unwrap()[0]
            .receipt
            .hash(),
        prior.hash()
    );
    assert_eq!(w.record_writes(&w.bob, &key), writes + 2);
    shop.receive(None).await.unwrap();
    let full = shop.app().protocol_scan("paykit", b"rc/").await.unwrap();
    assert_eq!(full.len(), 1);
    assert_eq!(
        full[0].0,
        format!("rc/{}", pubky_switch_core::enc::b64(&prior.hash())).as_bytes()
    );
    for count in 2..=8 {
        seller_wallet
            .app_mut()
            .delete_record(&target, Vec::new())
            .await
            .unwrap();
        shop.receive(None).await.unwrap();
        let shown = shop.retained_payee_receipts().await.unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].receipt.hash(), prior.hash());
        assert_eq!(shown[0].deletions.len(), count);
        assert_eq!(
            shop.app().protocol_scan("paykit", b"rc/").await.unwrap(),
            full
        );
        let ids = shop.app().protocol_scan("paykit", b"del/").await.unwrap();
        assert_eq!(ids.len(), count);
        assert!(ids
            .iter()
            .all(|(_, value)| value.len() < original.value().unwrap().len()));
        assert_eq!(shop.payee_deletion_notices().await.unwrap().len(), 1);
        seller_wallet
            .app_mut()
            .read_record_fresh(&target)
            .await
            .unwrap();
        seller_wallet
            .app_mut()
            .put_record(&target, original.value().unwrap(), Vec::new())
            .await
            .unwrap();
        shop.receive(None).await.unwrap();
    }
}

#[tokio::test]
async fn test_payee_deletion_without_a_copy_keeps_the_delete_through_reload() {
    let w = World::new();
    let scopes = || AppScopes::new().request().build().unwrap();
    let mut shop = w.shop().await;
    let mut seller_wallet = w.join(&w.bob, "seller-wallet", scopes()).await;
    let device = crate::common::store();
    let mut observer = w
        .join_with(
            &w.bob,
            "observer",
            scopes(),
            Default::default(),
            device.clone(),
        )
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let key = shop.app().record_key(&target).unwrap();
    seller_wallet.receive(None).await.unwrap();
    let previous = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.payee_deletion_notices().await.unwrap().is_empty());
    assert!(shop.retained_payee_receipts().await.unwrap().is_empty());
    let before = w.record_writes(&w.bob, &key);
    observer.app_mut().read_record_fresh(&target).await.unwrap();
    let (state_key, held) = observer
        .app()
        .protocol_scan(
            "paykit",
            format!("del/{}/", pubky_switch_core::enc::b64(&key)).as_bytes(),
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    let id = observer.app_id();
    drop(observer);
    let mut observer = w.load_on_clock(&w.bob, device, id, w.clock.clone()).await;
    observer.receive(None).await.unwrap();
    let mut expected: serde_json::Value = serde_json::from_slice(&held).unwrap();
    assert!(expected["d"]["page_time_ms"].is_null());
    expected["d"]["page_time_ms"] = w.clock.now_ms().into();
    let dated = observer
        .app()
        .protocol_get("paykit", &state_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&dated).unwrap(),
        expected
    );
    assert_eq!(w.record_writes(&w.bob, &key), before);
    assert!(observer.payee_activity().await.unwrap().is_empty());
    assert!(observer.payee_deletion_notices().await.unwrap().is_empty());
    let backup = observer.backup().await.unwrap();
    drop(observer);
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.bob, crate::common::store()),
        Default::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert!(restored
        .app()
        .protocol_get("paykit", &state_key)
        .await
        .unwrap()
        .is_some());
    assert_eq!(w.record_writes(&w.bob, &key), before);
    seller_wallet
        .app_mut()
        .put_record(&target, previous.value().unwrap(), Vec::new())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    let confirmed = paykit_sdk::ReceiptDetails::new()
        .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
    assert!(matches!(
        restored
            .confirm_payment_with(&key, confirmed)
            .await
            .unwrap_err()
            .refusal(),
        Some(Refusal::CreditHistoryLost)
    ));
    assert!(restored.retained_payee_receipts().await.unwrap().is_empty());
    assert_eq!(w.record_writes(&w.bob, &key), before + 1);
}

#[tokio::test]
async fn test_payee_deletion_an_older_receipt_survives_without_naming_the_writer() {
    let w = World::new();
    let device = crate::common::store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            Default::default(),
            device.clone(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.payee_activity().await.unwrap()[0].record;
    let receipt = shop.confirm_payment(&key).await.unwrap().unwrap();
    seller_wallet.receive(None).await.unwrap();
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let older = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let exact = shop.payee_deletion_notices().await.unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].writer, Some(seller_wallet.app_id()));
    let older_layer = shop.backup().await.unwrap();
    shop.acknowledge_payee_deletion(&exact[0]).await.unwrap();
    seller_wallet
        .app_mut()
        .read_record_fresh(&target)
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .put_record(&target, older.value().unwrap(), Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let mut newer: serde_json::Value = serde_json::from_slice(older.value().unwrap()).unwrap();
    newer["order"] = serde_json::json!("delivered");
    seller_wallet
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&newer).unwrap(), Vec::new())
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.app_mut().read_record_fresh(&target).await.unwrap();
    let notice = shop.payee_deletion_notices().await.unwrap();
    assert_eq!(notice.len(), 1);
    assert_eq!(notice[0].writer, None);
    assert_eq!(notice[0].page_time_ms, None);
    let receipts = shop.retained_payee_receipts().await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].receipt.message().msg_id, receipt);
    assert_eq!(receipts[0].deletions.len(), 2);
    let unknown = receipts[0]
        .deletions
        .iter()
        .find(|d| d.delete == notice[0].delete)
        .unwrap();
    assert_eq!(unknown.writer, None);
    assert_ne!(notice[0].delete, exact[0].delete);
    let count = w.record_writes(&w.bob, &key);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), count);
    let pending = shop.payee_deletion_notices().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].delete, notice[0].delete);
    seller_wallet
        .app_mut()
        .put_record(&target, older.value().unwrap(), Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    drop(shop);
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.bob, device),
        Default::default(),
        &older_layer,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.payee_deletion_notices().await.unwrap(), pending);
    let retained = restored.retained_payee_receipts().await.unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].deletions.len(), 2);
    restored
        .acknowledge_payee_deletion(&pending[0])
        .await
        .unwrap();
    assert!(restored.payee_deletion_notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_payee_deletion_in_place_restore_keeps_signed_receipt_after_a_later_write() {
    Box::pin(receipt_on_older_restore(BeforeRestore::Pending)).await;
}
#[tokio::test]
async fn test_payee_deletion_in_place_restore_keeps_acknowledged_warning() {
    Box::pin(receipt_on_older_restore(BeforeRestore::Acknowledged)).await;
}
#[tokio::test]
async fn test_payee_deletion_offline_installation_keeps_its_current_signed_receipt() {
    Box::pin(receipt_on_older_restore(BeforeRestore::Offline)).await;
}

#[tokio::test]
async fn test_payee_deletion_offline_restore_preserves_an_existing_credit_restriction() {
    Box::pin(receipt_on_older_restore(BeforeRestore::Marked)).await;
}

enum BeforeRestore {
    Pending,
    Acknowledged,
    Offline,
    Marked,
}

async fn receipt_on_older_restore(when: BeforeRestore) {
    let w = World::new();
    let device = crate::common::store();
    let scopes = || AppScopes::new().request().build().unwrap();
    let mut shop = w
        .join_with(&w.bob, "shop", scopes(), Default::default(), device.clone())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w.join(&w.bob, "seller-wallet", scopes()).await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;
    let mut backup = shop.backup().await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.payee_activity().await.unwrap()[0].record;
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let before = shop
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    let issued = if matches!(when, BeforeRestore::Marked) {
        let mut activity = paykit_lib::records::ActivityRecord::read(
            before.value().unwrap(),
            &w.bob_id(),
            paykit_lib::records::ActivitySide::Payee,
        )
        .unwrap();
        activity.lost_to_delete = Some([17; 32]);
        shop.app_mut()
            .put_record(&target, &activity.to_json().unwrap(), Vec::new())
            .await
            .unwrap();
        None
    } else {
        Some(shop.confirm_payment(&key).await.unwrap().unwrap())
    };
    seller_wallet.receive(None).await.unwrap();
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    let mut observed = None;
    if matches!(when, BeforeRestore::Pending | BeforeRestore::Acknowledged) {
        shop.receive(None).await.unwrap();
        let notices = shop.payee_deletion_notices().await.unwrap();
        assert_eq!(notices.len(), 1);
        if matches!(when, BeforeRestore::Acknowledged) {
            backup = shop.backup().await.unwrap();
            shop.acknowledge_payee_deletion(&notices[0]).await.unwrap();
        }
        seller_wallet
            .app_mut()
            .read_record_fresh(&target)
            .await
            .unwrap();
        observed = Some(notices[0].clone());
    }
    seller_wallet
        .app_mut()
        .put_record(&target, before.value().unwrap(), Vec::new())
        .await
        .unwrap();
    if matches!(when, BeforeRestore::Pending | BeforeRestore::Acknowledged) {
        shop.receive(None).await.unwrap();
    }
    drop(shop);
    let count = w.record_writes(&w.bob, &key);
    if !matches!(when, BeforeRestore::Acknowledged) {
        let mut fresh = paykit_sdk::Paykit::restore(
            w.env(&w.bob, crate::common::store()),
            Default::default(),
            &backup,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
        assert!(fresh.retained_payee_receipts().await.unwrap().is_empty());
        fresh.receive(None).await.unwrap();
        assert!(fresh.retained_payee_receipts().await.unwrap().is_empty());
    }
    let mut restored = paykit_sdk::Paykit::restore(
        w.env(&w.bob, device),
        Default::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let retained = restored.retained_payee_receipts().await.unwrap();
    if let Some(id) = issued {
        assert_eq!(
            retained.len(),
            1,
            "the existing installation still held the receipt"
        );
        assert_eq!(retained[0].receipt.message().msg_id, id);
    } else {
        assert!(retained.is_empty());
    }
    if let Some(notice) = observed {
        assert_eq!(retained[0].deletions.len(), 1);
        assert_eq!(retained[0].deletions[0].delete, notice.delete);
        let warnings = restored.payee_deletion_notices().await.unwrap();
        if matches!(when, BeforeRestore::Pending) {
            assert_eq!(warnings, vec![notice]);
        } else {
            assert!(warnings.is_empty());
        }
    } else {
        assert!(retained.iter().all(|r| r.deletions.is_empty()));
        assert!(restored.payee_deletion_notices().await.unwrap().is_empty());
    }
    restored.receive(None).await.unwrap();
    let confirmed = paykit_sdk::ReceiptDetails::new()
        .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
    let result = restored.confirm_payment_with(&key, confirmed).await;
    if matches!(when, BeforeRestore::Offline) {
        assert_eq!(result.unwrap(), None);
    } else {
        assert!(matches!(
            result.unwrap_err().refusal(),
            Some(Refusal::CreditHistoryLost)
        ));
    }
    assert_eq!(w.record_writes(&w.bob, &key), count);
}

#[tokio::test]
async fn test_payee_deletion_receipt_conflict_checks_the_payment_again() {
    Box::pin(confirm_after_conflict(false)).await;
}

#[tokio::test]
async fn test_payee_deletion_receipt_conflict_rejects_a_different_signed_proof() {
    Box::pin(confirm_after_conflict(true)).await;
}

async fn confirm_after_conflict(changed_proof: bool) {
    use crate::common::{endpoint, store, ReadHook};
    use pubky_switch_client::transport::Transport;
    use std::sync::Arc;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let reads = Arc::new(ReadHook::new(Arc::clone(&w.router)));
    let scopes = || AppScopes::new().request().build().unwrap();
    let mut shop = w
        .join_through(
            &w.bob,
            "shop",
            scopes(),
            Default::default(),
            store(),
            Arc::clone(&reads) as Arc<dyn Transport>,
        )
        .await;
    let mut seller_wallet = w.join(&w.bob, "seller-wallet", scopes()).await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let key = shop.app().record_key(&target).unwrap();
    let prior = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    let mut record = paykit_lib::records::ActivityRecord::read(
        prior.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    if changed_proof {
        let source = record.proof.as_ref().unwrap();
        let paykit_lib::Body::PaymentProof(mut body) = source.body().clone() else {
            panic!("payment proof")
        };
        body.attempt_id = paykit_lib::AttemptId::from_bytes([33; 16]);
        let refs = source.message().refs.clone();
        let signed = w.sign_as(
            &w.alice,
            "wallet",
            &w.bob,
            "paykit.payment_proof",
            refs.clone(),
            body.to_body(refs.len()).unwrap(),
        );
        record.proof = Some(
            paykit_lib::records::StoredMessage::new(
                w.alice_id(),
                wallet.app().cert().signed(),
                signed,
                Some(w.clock.now_ms()),
                &w.bob_id(),
            )
            .unwrap(),
        );
    }
    let mut winning: serde_json::Value =
        serde_json::from_slice(&record.to_json().unwrap()).unwrap();
    winning["counter_extension"] = serde_json::json!("retained");
    seller_wallet
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&winning).unwrap(), Vec::new())
        .await
        .unwrap();
    let writes = w.record_writes(&w.bob, &key);
    let rail = Arc::clone(&w.rail);
    let count = rail.proof_checks().len();
    if changed_proof {
        assert!(matches!(
            shop.confirm_payment(&key).await.unwrap_err().refusal(),
            Some(paykit_sdk::Refusal::NotNow(_))
        ));
        let checks = &w.rail.proof_checks()[count..];
        assert_eq!(checks.len(), 1);
        assert_ne!(
            checks[0].attempt_id,
            paykit_lib::AttemptId::from_bytes([33; 16])
        );
        assert!(
            shop.app()
                .shared_record(&key)
                .await
                .unwrap()
                .unwrap()
                .flagged
        );
        assert_eq!(w.record_writes(&w.bob, &key), writes);
        assert_eq!(w.rail.pay_calls(&checks[0].attempt_id), 1);
        wallet.receive(None).await.unwrap();
        assert!(wallet
            .receipts(paykit_sdk::ReceiptsOf::All)
            .await
            .unwrap()
            .is_empty());
        return;
    }
    let path = format!("records/{}", pubky_switch_core::enc::b64(&key));
    reads.after_read(&path, move || {
        rail.set_fraction_digits(endpoint("btc-lightning-bolt11"), Some(0))
    });
    assert_eq!(shop.confirm_payment(&key).await.unwrap(), None);
    assert!(reads.fired());
    let checks = &w.rail.proof_checks()[count..];
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].proof, checks[1].proof);
    assert_eq!(checks[0].attempt_id, checks[1].attempt_id);
    assert_ne!(checks[0].amount, checks[1].amount);
    assert_eq!(w.record_writes(&w.bob, &key), writes);
    assert_eq!(w.rail.executions(&checks[0].attempt_id), 1);
    assert_eq!(w.rail.pay_calls(&checks[0].attempt_id), 1);
    let held = shop
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    let held: serde_json::Value = serde_json::from_slice(held.value().unwrap()).unwrap();
    assert!(held["receipt"].is_null());
    assert_eq!(held["counter_extension"], "retained");
    w.rail
        .set_fraction_digits(endpoint("btc-lightning-bolt11"), Some(8));
    let receipt = shop.confirm_payment(&key).await.unwrap().unwrap();
    assert_eq!(
        shop.confirm_payment(&key).await.unwrap().as_ref(),
        Some(&receipt)
    );
    let latest = shop
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    let verified = paykit_lib::records::ActivityRecord::read(
        latest.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    let proof = verified.proof.as_ref().unwrap();
    let settled = verified.receipt.as_ref().unwrap();
    assert_eq!(settled.message().msg_id, receipt);
    assert_eq!(
        settled.message().refs,
        vec![proof.hash(), request.request_hash]
    );
    assert!(
        matches!(settled.body(),paykit_lib::Body::PaymentReceipt(r) if r.attempt_id == checks[0].attempt_id)
    );
    assert_eq!(w.record_writes(&w.bob, &key), writes + 1);
    assert_eq!(w.rail.pay_calls(&checks[0].attempt_id), 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .receipts(paykit_sdk::ReceiptsOf::All)
            .await
            .unwrap()
            .len(),
        1
    );
}
