//! Merchant wallets confirming distinct requests against a shared record quota.

use super::*;
use paykit_lib::records::{ActivityRecord, ActivitySide};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
use std::sync::Mutex;

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
    let merchant = wallets.seller_wallet.identity();
    Box::pin(finish_seller_lookups(&mut wallets)).await?;

    let [(transaction, tag)] =
        transactions_in_one_bucket(wallets.seller_wallet.app().naming_key())?;
    wallets.rail.next(Behaviour::InTransaction(transaction));
    let (mut buyer, winner, winning_key) = Box::pin(paid_request(
        &mut wallets.seller_wallet,
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
        .seller_wallet
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
    let unused_key = wallets.seller_wallet.app().record_key(&unused_target)?;
    wallets
        .seller_wallet
        .cancel_sent_request(&unused_key, None)
        .await?;
    let number = settlement_bucket(&tag);
    let key = SettlementBucket::record_key(number, wallets.seller_wallet.app().naming_key())?;
    let caller = pubky_switch_core::enc::zbase32_encode(&wallets.labelled.app_id());
    let ops_path = format!("/v1/{}/ops", identity.key().id());
    let quota_answers = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&quota_answers);
    // The trace abbreviates the budget name. Read it from this wallet's actual answer.
    world.distort(move |request, answer| {
        if request.origin == origin
            && request.method == "POST"
            && request.path_and_query == ops_path
            && request.header(pubky_switch_core::http::header::APP) == Some(caller.as_str())
            && answer.status == 429
        {
            observed.lock().unwrap().push(answer.body.clone());
        }
    });
    let start = world.trace().lines.len();
    let checks_before_quota = wallets.rail.proof_checks().len();
    let refused = wallets
        .labelled
        .confirm_payment(&losing_key)
        .await
        .unwrap_err();
    let trace = world.trace();
    let mut refusals = trace.lines[start..].iter().filter(|line| {
        line.call == Some("ops")
            && line.label == "{shop/labelled-wallet} POST /v1/{shop}/ops"
            && line.facts.get("status").map(String::as_str) == Some("429")
    });
    let refusal = refusals
        .next()
        .ok_or_else(|| format!("missing merchant's service quota refusal: {refused:?}"))?;
    assert!(refusals.next().is_none());
    assert!(refusal.facts.contains_key("answer signed with"));
    assert_eq!(
        refusal.facts.get("body.code").map(String::as_str),
        Some("text quota_exceeded")
    );
    let answers = std::mem::take(&mut *quota_answers.lock().unwrap());
    let [answer] = answers.as_slice() else {
        return Err("expected one quota answer from the merchant service".into());
    };
    let body: serde_json::Value = serde_json::from_slice(answer)?;
    assert_eq!(body["code"], "quota_exceeded");
    assert_eq!(body["current"]["limit"], "record_keys");
    assert_eq!(
        refused.refusal(),
        Some(&paykit_sdk::Refusal::SettlementBucketFull { bucket: number }),
        "after the merchant's signed record quota response: {refused:?}"
    );
    let method_checks = wallets.rail.proof_checks();
    let [accepted_check] = &method_checks[checks_before_quota..] else {
        return Err("expected one verified proof before the quota refusal".into());
    };

    assert!(wallets.labelled.app().shared_record(&key).await?.is_none());

    // Removing the unrelated, cancelled invoice leaves room for the settlement mark.
    wallets
        .seller_wallet
        .app_mut()
        .delete_record(&unused_target, vec![])
        .await?;
    let receipt = wallets
        .seller_wallet
        .confirm_payment(&winning_key)
        .await?
        .ok_or("missing winning receipt")?;
    Box::pin(confirm_after_other_credit(
        &mut wallets.labelled,
        &losing_key,
        &wallets.rail,
    ))
    .await?;
    assert_eq!(wallets.rail.proof_checks().last(), Some(accepted_check));

    assert!(wallets
        .labelled
        .app()
        .queued_messages("paykit")
        .await?
        .iter()
        .all(|m| m.kind != paykit_lib::Kind::PaymentReceipt.as_str()));

    let current = wallets
        .seller_wallet
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing settlement")?;
    let naming = wallets.seller_wallet.app().naming_key();
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
    let stored = receipt_hash(&wallets.seller_wallet, &winning_key, ActivitySide::Payee).await?;
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
