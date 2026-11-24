//! A payee app keeps the signed receipts it held across a restore, and checks a payment
//! again after a conflicting write (Paykit spec 6, "Verified receipts").

use crate::common::{draft, request_from, World};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_sdk::{AppScopes, PayOptions, Refusal};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
use pubky_switch_client::clock::Clock;

#[tokio::test]
async fn test_payee_receipt_in_place_restore_keeps_the_installations_signed_receipt() {
    Box::pin(receipt_on_older_restore(BeforeRestore::Issued)).await;
}

#[tokio::test]
async fn test_payee_receipt_in_place_restore_preserves_an_existing_credit_restriction() {
    Box::pin(receipt_on_older_restore(BeforeRestore::Marked)).await;
}

enum BeforeRestore {
    Issued,
    Marked,
}

/// Another own app writes the record back to its value before the receipt; the shop is
/// then restored from a backup older than the receipt, in place and on a fresh device.
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
    let backup = shop.backup().await.unwrap();
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
        .put_record(&target, before.value().unwrap(), Vec::new())
        .await
        .unwrap();
    drop(shop);
    let count = w.record_writes(&w.bob, &key);
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
    restored.receive(None).await.unwrap();
    let confirmed = paykit_sdk::ReceiptDetails::new()
        .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
    let result = restored.confirm_payment_with(&key, confirmed).await;
    if matches!(when, BeforeRestore::Issued) {
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
async fn test_payee_receipt_conflict_checks_the_payment_again() {
    Box::pin(confirm_after_conflict(false)).await;
}

#[tokio::test]
async fn test_payee_receipt_conflict_rejects_a_different_signed_proof() {
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
            Some(paykit_sdk::Refusal::HeldForUser(_))
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
