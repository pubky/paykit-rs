//! Invitation references follow delivered requests without authorizing a payment.

use super::*;
use paykit_sdk::{MessageDelivery, Refusal, RequestStatus, SentRequest};
use pubky_switch_client::app::AppEvent;

#[tokio::test]
async fn test_sibling_wallet_reads_invitations_without_granting_request_scope() -> TestResult {
    invited_requests(false).await
}

#[tokio::test]
async fn test_restored_wallet_reads_invitations_created_after_its_backup() -> TestResult {
    invited_requests(true).await
}

async fn invited_requests(restore_creator: bool) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(delivered_requests(&world, restore_creator)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn delivered_requests(world: &World, restore_creator: bool) -> TestResult {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut buyer_signer =
        Box::pin(setup_identity(world, "buyer", "https://buyer.payment.test")).await?;
    let mut creator = Box::pin(join(
        world,
        &mut buyer_signer,
        "buyer",
        "invite-creator",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let mut receiver = Box::pin(join(
        world,
        &mut buyer_signer,
        "buyer",
        "receiving-wallet",
        AppScopes::new().pay().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    // The creator's saved state contains neither invitation nor any host note about it.
    let backup = creator.backup().await?;
    let first_code = creator
        .app_mut()
        .register_invite_code(world.now_ms() + 3_600_000, 1)
        .await?;
    let second_code = creator
        .app_mut()
        .register_invite_code(world.now_ms() + 3_600_000, 1)
        .await?;
    drop(creator);

    let mut seller_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let label = paykit_lib::Label::new("checkout")?;
    let mut seller = Box::pin(join(
        world,
        &mut seller_signer,
        "shop",
        "labelled-seller",
        AppScopes::new().request_label(&label).build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let request = |reference: &str| -> TestResult<RequestDraft> {
        Ok(RequestDraft::new(
            PaymentAmount {
                asset: Asset::new("btc")?,
                value: Decimal::new("0.00025")?,
            },
            PaymentReference::new(reference)?,
            RequestDestination::Endpoints(BTreeMap::from([(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
                PaymentEndpointPayload::new("invoice")?,
            )])),
        ))
    };
    // Holding the payer's code does not let another app of the seller act as a payee.
    let mut payer_only = Box::pin(join(
        world,
        &mut seller_signer,
        "shop",
        "payer-only",
        AppScopes::new().pay().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let before = world.requests_sent();
    let refused = Box::pin(payer_only.send_request(
        receiver.identity(),
        request("unpermitted")?.with_invite_code(first_code.code.clone()),
    ))
    .await
    .unwrap_err();
    assert!(matches!(
        refused.refusal(),
        Some(Refusal::ScopeMissing(scope)) if scope == "paykit_request or paykit_request:<label>"
    ));
    assert_eq!(world.requests_sent(), before);
    assert!(payer_only.app().queued_messages("paykit").await?.is_empty());
    drop(payer_only);

    // The first append applied but its answer was lost. Flush retries the same request
    // and envelope, even though its single-use invitation has already been redeemed.
    // Its success also proves that the refused app did not spend this invitation.
    world.lose_answer("POST", "/inbox");
    let first = Box::pin(seller.send_request(
        receiver.identity(),
        request("first")?.with_invite_code(first_code.code.clone()),
    ))
    .await?;
    assert!(matches!(first.delivery, MessageDelivery::Queued { .. }));
    assert_eq!(world.faults_left(), 0);
    seller.flush().await?;
    assert!(matches!(
        seller.message_delivery(&first.msg_id).await?,
        Some(MessageDelivery::Accepted { .. })
    ));
    let second = Box::pin(seller.send_request(
        receiver.identity(),
        request("second")?.with_invite_code(second_code.code.clone()),
    ))
    .await?;
    assert!(matches!(second.delivery, MessageDelivery::Accepted { .. }));
    let ordinary = Box::pin(seller.send_request(receiver.identity(), request("ordinary")?)).await?;
    assert!(matches!(
        ordinary.delivery,
        MessageDelivery::Accepted { .. }
    ));
    assert_ne!(first.request, second.request);
    assert_ne!(first.msg_id, second.msg_id);
    assert_eq!(seller.payee_activity().await?.len(), 3);

    if restore_creator {
        receiver = Box::pin(Paykit::restore(
            world.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            &backup,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
    }
    let expected = [
        (&first, Some(first_code.code_hash), "first"),
        (&second, Some(second_code.code_hash), "second"),
        (&ordinary, None, "ordinary"),
    ];
    Box::pin(read_invitations(&mut receiver, &expected, label.as_str())).await?;
    assert!(receiver.payments().await?.is_empty());
    assert!(rail.all_pay_calls().is_empty());
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn read_invitations(
    receiver: &mut Paykit,
    expected: &[(&SentRequest, Option<[u8; 32]>, &str)],
    label: &str,
) -> TestResult {
    let mut received = BTreeMap::new();
    for _ in 0..8 {
        let read = receiver.receive(None).await?;
        for event in &read.report.events {
            if let AppEvent::MessageReceived {
                msg_id, receipt, ..
            } = event
            {
                assert!(received
                    .insert(msg_id.clone(), receipt.invite_code_hash)
                    .is_none());
            }
        }
        if received.len() == expected.len() && read.records_unlisted.is_none() {
            break;
        }
    }
    assert_eq!(received.len(), expected.len());
    for (sent, invite, reference) in expected {
        assert_eq!(received.get(&sent.msg_id), Some(invite));
        let request = sent.request.ok_or("request was not recorded")?;
        let view = receiver.request(&request).await?;
        assert_eq!(view.request, request);
        assert_eq!(view.payment_reference, *reference);
        assert_eq!(view.label.as_deref(), Some(label));
        assert_eq!(view.status, RequestStatus::Open);
    }
    Ok(())
}
