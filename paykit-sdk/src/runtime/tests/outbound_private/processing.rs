use super::super::*;

#[tokio::test]
async fn test_process_outbound_private_messages_waits_for_retry_without_changing_state() {
    let backoff = ChronoDuration::from_std(OUTBOUND_PRIVATE_RETRY_BACKOFF).unwrap();
    for (elapsed, due) in [
        (backoff - ChronoDuration::milliseconds(1), false),
        (backoff, true),
    ] {
        let storage = registered_test_storage();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction(|tx| {
                let event = payment_request_message(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                    "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
                    None,
                );
                let mut message =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id(),
                        event.kind.unwrap(),
                        event.raw_json,
                        FixedClock.now() - elapsed,
                    ))?;
                message.last_attempt_at = Some(FixedClock.now() - elapsed);
                message.attempt_count = 1;
                message = mark_outbound_sent(message, FixedClock.now() - elapsed);
                tx.save_outbound_private_message(message)?;
                Ok(())
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("bitkit").unwrap(),
            FixedClock,
        );
        let before = storage.snapshot().unwrap();
        let result = sdk.process_outbound_private_messages(counterparty).await;
        if due {
            assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        } else {
            assert_eq!(result.unwrap(), OutboundPrivateSendReport::default());
            assert_eq!(storage.snapshot().unwrap(), before);
        }
    }
}

#[tokio::test]
async fn test_process_pending_private_messages_reports_counterparty_errors() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let reports = sdk.process_pending_private_messages().await.unwrap();

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].counterparty, counterparty);
    assert!(reports[0].report.is_none());
    assert!(reports[0].error.is_some());
}

#[tokio::test]
async fn test_process_outbound_private_messages_preserves_untrusted_queue_without_session() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::domain::outbound_private::enqueue_private_message(
        &storage,
        counterparty.clone(),
        private_list_json(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let queued =
        crate::domain::outbound_private::queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
    assert_eq!(queued.len(), 1);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_process_outbound_private_messages_blocks_recovery_required_peer() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::domain::linked_peers::save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::RecoveryRequired,
        FixedClock.now(),
    )
    .await
    .unwrap();
    crate::domain::outbound_private::enqueue_private_message(
        &storage,
        counterparty.clone(),
        private_list_json(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone())
        .await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
    let queued =
        crate::domain::outbound_private::queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
    assert_eq!(queued.len(), 1);
}
