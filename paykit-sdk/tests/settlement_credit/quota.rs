//! Merchant wallets confirming distinct requests against a shared record quota.

use super::*;
use paykit_lib::records::{ActivityRecord, ActivitySide};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};

#[tokio::test]
async fn test_settlement_credit_after_a_record_quota_refusal_keeps_one_receipt() -> TestResult {
    let Some(world) = World::start(Backend::Service)? else {
        return Ok(());
    };
    let result = Box::pin(confirm_after_quota(&world)).await;
    world.close().await?;
    result
}

async fn confirm_after_quota(world: &World) -> TestResult {
    // Two paid requests and a cancelled invoice occupy the merchant's three record slots.
    let origin = "https://shop.payment.test";
    let identity = world.identity("shop")?;
    let limits = serde_json::from_value(serde_json::json!({"max_record_keys": 3}))?;
    world.add_switch_with(origin, &[&identity], limits).await?;
    let signer = Box::pin(signer_on(world, &identity, origin)).await?;
    let mut wallets = Box::pin(enrolled_merchant_wallets(world, Some(signer))).await?;
    let merchant = wallets.till.identity();
    Box::pin(finish_seller_lookups(&mut wallets)).await?;

    let [(transaction, tag)] = transactions_in_one_bucket(wallets.till.app().naming_key())?;
    wallets.rail.next(Behaviour::InTransaction(transaction));
    let (mut buyer, winner, winning_key) = Box::pin(paid_request(
        &mut wallets.till,
        wallets.buyer,
        "btc-bitcoin-p2wpkh",
        "bcrt1qqyqszqgpqyqszqgpqyqszqgpqyqszqgpvxat9t",
    ))
    .await?;
    wallets.rail.next(Behaviour::InTransaction(transaction));
    let (mut other, loser, losing_key) = Box::pin(paid_request(
        &mut wallets.labelled,
        wallets.other,
        "btc-bitcoin-p2wpkh",
        "bcrt1qqgpqyqszqgpqyqszqgpqyqszqgpqyqszazmwwa",
    ))
    .await?;
    assert_ne!(winning_key, losing_key);
    let executions = wallets.rail.all_executions();
    assert_eq!(executions.len(), 2);
    assert!(executions.values().all(|count| *count == 1));
    assert_eq!(wallets.rail.all_pay_calls(), executions);

    let unused = wallets
        .till
        .send_request(
            buyer.identity(),
            invoice("btc-lightning-bolt11", "ln-unused")?,
        )
        .await?
        .request
        .ok_or("missing unused request")?;
    let collection = Collection::PayeeActivity;
    let unused_target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.fixed_policy()),
        &natural_key::request_record(&merchant, &unused.request_hash),
    );
    let unused_key = wallets.till.app().record_key(&unused_target)?;
    wallets.till.cancel_sent_request(&unused_key, None).await?;
    let number = settlement_bucket(&tag);
    let key = SettlementBucket::record_key(number, wallets.till.app().naming_key())?;
    let start = world.trace().lines.len();
    let refused = wallets
        .labelled
        .confirm_payment(&losing_key)
        .await
        .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&paykit_sdk::Refusal::SettlementBucketFull { bucket: number })
    );
    let trace = world.trace();
    let mut refusals = trace.lines[start..].iter().filter(|line| {
        line.call == Some("ops")
            && line.label.starts_with("{shop/labelled-till} POST ")
            && line.facts.get("status").map(String::as_str) == Some("429")
    });
    let refusal = refusals
        .next()
        .ok_or("missing merchant's service quota refusal")?;
    assert!(refusals.next().is_none());
    assert!(refusal.facts.contains_key("answer signed with"));
    assert_eq!(
        refusal.facts.get("body.code").map(String::as_str),
        Some("text quota_exceeded")
    );
    assert_eq!(
        refusal.facts.get("body.current.limit").map(String::as_str),
        Some("text record_keys")
    );

    assert!(wallets.labelled.app().shared_record(&key).await?.is_none());

    // Removing the unrelated, cancelled invoice leaves room for the settlement mark.
    wallets
        .till
        .app_mut()
        .delete_record(&unused_target, vec![])
        .await?;
    let receipt = wallets
        .till
        .confirm_payment(&winning_key)
        .await?
        .ok_or("missing winning receipt")?;
    Box::pin(confirm_after_other_credit(
        &mut wallets.labelled,
        &losing_key,
        &wallets.rail,
    ))
    .await?;

    assert!(wallets
        .labelled
        .app()
        .queued_messages("paykit")
        .await?
        .iter()
        .all(|m| m.kind != paykit_lib::Kind::PaymentReceipt.as_str()));

    let current = wallets
        .till
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing settlement")?;
    let naming = wallets.till.app().naming_key();
    let bucket = SettlementBucket::bind(
        current.content.value().ok_or("unreadable settlement")?,
        number,
        naming,
        &key,
    )?;
    let occurrence = occurrence_tag(
        naming,
        &natural_key::request_record(&merchant, &winner.request_hash),
    );
    assert_eq!(
        bucket.marks(),
        &[SettlementMark::new(
            &tag,
            OccurrenceKind::OneTimeRequest,
            occurrence
        )]
    );
    let stored = receipt_hash(&wallets.till, &winning_key, ActivitySide::Payee).await?;
    buyer.receive(None).await?;
    other.receive(None).await?;
    let received = buyer.receipts(ReceiptsOf::Request(winner)).await?;
    let [received] = received.as_slice() else {
        return Err("the winning buyer should hold one receipt".into());
    };
    assert_eq!(received.msg_id, receipt);
    assert_eq!(received.request_hash, Some(winner.request_hash));
    assert!(executions.contains_key(&received.attempt_id));
    assert_eq!(
        receipt_hash(&buyer, &received.record, ActivitySide::Payer).await?,
        stored
    );
    assert!(other.receipts(ReceiptsOf::Request(loser)).await?.is_empty());
    assert_eq!(wallets.rail.all_pay_calls(), executions);
    assert_eq!(wallets.rail.all_executions(), executions);
    Ok(())
}

async fn receipt_hash(wallet: &Paykit, key: &[u8; 32], side: ActivitySide) -> TestResult<[u8; 32]> {
    let record = wallet
        .app()
        .shared_record(key)
        .await?
        .ok_or("missing receipt record")?;
    let activity = ActivityRecord::read(
        record.content.value().ok_or("unreadable receipt record")?,
        &wallet.identity(),
        side,
    )?;
    Ok(activity.receipt.ok_or("missing signed receipt")?.hash())
}
