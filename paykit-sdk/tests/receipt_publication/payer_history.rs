use super::*;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::RequestStatus;

#[tokio::test]
async fn test_enrolled_buyer_keeps_the_receipt_and_refuses_a_distinct_marked_request() -> TestResult
{
    with_issued_sale(marked_request).await
}

async fn marked_request(_world: &World, sale: IssuedSale) -> TestResult {
    let IssuedSale {
        mut buyer,
        mut shop,
        request: paid,
        receipt,
        rail,
        ..
    } = sale;
    let calls = rail.all_pay_calls();
    assert_eq!(calls.values().sum::<u64>(), 1);
    let draft = invoice("btc-lightning-bolt11", "ln-other-request")?;
    assert!(draft.payment_deadline_ms.is_none() && draft.proposal_expires_at_ms.is_none());
    let request = shop
        .send_request(buyer.identity(), draft)
        .await?
        .request
        .ok_or("request was not sent")?;
    assert_ne!(request, paid);
    buyer.receive(None).await?;
    let paid_view = buyer.request(&paid).await?;
    assert_eq!(paid_view.status, RequestStatus::Paid);
    let prior = buyer
        .app()
        .shared_record(&paid_view.record)
        .await?
        .ok_or("original payer record missing")?;
    let paid_record = ActivityRecord::bind(
        prior
            .content
            .value()
            .ok_or("original payer record unreadable")?,
        &buyer.identity(),
        ActivitySide::Payer,
        buyer.app().naming_key(),
        &paid_view.record,
        None,
        &|_, proof| paykit_sdk::rail::fake_settlement_id(proof),
    )?;
    assert_eq!(
        paid_record
            .receipt
            .ok_or("original receipt missing")?
            .hash(),
        receipt.hash()
    );
    let view = buyer.request(&request).await?;
    assert_eq!(view.status, RequestStatus::Open);

    buyer.block(&shop.identity()).await?;
    let refusal = Box::pin(buyer.pay_request(&request, PayOptions::default()))
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::Blocked { payment }) = refusal.refusal() else {
        return Err(format!("expected the bound blocked-payment prompt, got {refusal:?}").into());
    };
    let consent = PayOptions::default().with_blocked_confirmed((**payment).clone());
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        DispatchCollection::new(collection.name(), collection.fixed_policy()),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    let original = buyer
        .app()
        .shared_record(&view.record)
        .await?
        .ok_or("missing payer record")?;
    let mut marked = ActivityRecord::bind(
        original.content.value().ok_or("missing request")?,
        &buyer.identity(),
        ActivitySide::Payer,
        buyer.app().naming_key(),
        &view.record,
        None,
        &|_, proof| paykit_sdk::rail::fake_settlement_id(proof),
    )?;
    marked.lost_to_delete = Some([36; 32]);
    buyer
        .app_mut()
        .put_record(&target, &marked.to_json()?, Vec::new())
        .await?;
    let obligation = paykit_lib::derive::request_obligation(
        buyer.app().naming_key(),
        &request.payee,
        &request.request_hash,
        0,
    );
    assert!(buyer.app_mut().read_claim(&obligation).await?.is_none());
    rail.next(Behaviour::Refuse);
    let refused = Box::pin(buyer.pay_request(&request, consent))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::RecordHeld(_))),
        "{refused:?}"
    );
    let view = buyer.request(&request).await?;
    assert_eq!(view.status, RequestStatus::MayBePaid);
    assert!(view.held.is_none() && view.receipt.is_none());
    assert!(buyer.app_mut().read_claim(&obligation).await?.is_none());
    assert_eq!(rail.all_pay_calls(), calls);
    shop.receive(None).await?;
    let sales = shop.payee_activity().await?;
    assert_eq!(
        sales
            .iter()
            .filter(|sale| sale.status == PayeeStatus::Receipted)
            .count(),
        1
    );
    assert!(sales
        .iter()
        .any(|sale| sale.request_hash == Some(request.request_hash)
            && sale.status == PayeeStatus::Requested));
    Ok(())
}
