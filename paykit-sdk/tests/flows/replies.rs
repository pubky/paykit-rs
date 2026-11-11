//! A payer's replies that reach the payee before it holds the request record they name are
//! kept until it does, then placed (Switch spec 8.2, Paykit spec 8.1).

use std::sync::Arc;

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Rail, RequestRef};
use pubky_switch_client::app::{
    Collection as SwitchCollection, OpBatch, OutgoingMessage, RecordTarget, SignedMessage,
};
use serde_json::json;

use crate::common::{store, World};

/// Bob's shop signs a request to alice and sends it alone, without writing its record, as
/// another of bob's apps would before this one holds the record. Alice pays it: her
/// acceptance and proof reach the shop. Returns the request.
async fn paid_before_recorded(w: &World, wallet: &mut Paykit, shop: &mut Paykit) -> SignedMessage {
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    let signed = shop
        .app()
        .sign_message(OutgoingMessage::new(
            w.alice_id(),
            "paykit.payment_request",
            body.as_object().cloned().unwrap(),
        ))
        .unwrap();
    shop.app_mut().queue_signed(signed.clone()).await.unwrap();
    shop.app_mut().flush_outbox().await.unwrap();
    wallet.receive(None).await.unwrap();
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: signed.hash(),
    };
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap().is_empty());
    signed
}

/// The shop's record of `request` is written, as the other app's batch would write it.
async fn record_request(w: &World, shop: &mut Paykit, request: &SignedMessage) {
    let copy = StoredMessage::new(
        w.bob_id(),
        shop.app().cert().signed(),
        request.plaintext().clone(),
        None,
        &w.alice_id(),
    )
    .unwrap();
    let mut record = ActivityRecord::new(ActivitySide::Payee, w.alice_id());
    record.msg_ids = vec![copy.message().msg_id.clone()];
    record.request = Some(copy);
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.policy(None),
        ),
        &natural_key::request_record(&w.bob_id(), &request.hash()),
    );
    let value = record.to_json().unwrap();
    let entry = shop
        .app_mut()
        .record_op(&target, Some(&value), Vec::new())
        .await
        .unwrap();
    shop.app_mut()
        .submit_batch(OpBatch::new().entry(entry))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_replies_kept_until_the_record_is_held_then_placed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    // A backup carries the kept replies.
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    shop.reconcile().await.unwrap();
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_replies_kept_no_longer_than_their_sender_keeps_them() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_REPLY_MS);
    // A read past the bound drops them; the record that comes later holds the request alone.
    shop.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::Requested);
}
