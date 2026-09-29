use super::*;

async fn append_end(fixture: &Fixture, sent: bool, inbound: bool) {
    let record = crate::domain::allowances::allowance_record(
        &fixture.storage,
        &fixture.occurrence.request.counterparty,
        &path(),
        &fixture.allowance,
    )
    .await
    .unwrap()
    .unwrap();
    let event = paykit_lib::AllowanceEvent::End(
        paykit_lib::AllowanceEnd::accepted(
            paykit_lib::EventId::new_v4(),
            fixture.allowance.clone(),
            paykit_lib::EventId::new(record.proposal_event_id.unwrap()).unwrap(),
            paykit_lib::EventId::new(record.acceptance_event_id.unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let raw = paykit_lib::serialize_allowance_event(&event).unwrap();
    if inbound {
        crate::domain::private_stream::persist_private_stream_batch(
            &fixture.storage,
            fixture.occurrence.request.counterparty.clone(),
            path(),
            vec![message(raw)],
            None,
            time(),
        )
        .await
        .unwrap();
    } else {
        fixture
            .storage
            .transaction(|tx| {
                let mut record = tx.insert_outbound_private_message(
                    crate::storage::NewOutboundPrivateMessage::new(
                        fixture.occurrence.request.counterparty.clone(),
                        path(),
                        "paykit.allowance_end".into(),
                        raw,
                        time(),
                    ),
                );
                if sent {
                    record.status = crate::OutboundPrivateMessageStatus::Sent;
                    record.sent_at = Some(time());
                    tx.save_outbound_private_message(record)?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn test_accounting_restore_cannot_erase_end_or_reauthorize_payment() {
    for (sent, inbound) in [(false, false), (true, false), (false, true)] {
        let fixture = Fixture::new().await;
        let backup = crate::export_backup_state(&fixture.storage, path())
            .await
            .unwrap();
        append_end(&fixture, sent, inbound).await;
        let before = fixture.storage.snapshot().unwrap();
        let error = crate::backup::restore_backup_state(&fixture.storage, backup)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("discard retained Allowance or Payment Request history"));
        assert_eq!(fixture.storage.snapshot().unwrap(), before);
        let revision = fixture.state().revision;
        fixture
            .storage
            .transaction(|tx| {
                reconcile(
                    tx,
                    &path(),
                    AllowanceAccountingReconciliation {
                        expected_revision: Some(revision),
                        history: Default::default(),
                        outcomes: vec![],
                        trusted_time: time(),
                    },
                )
            })
            .await
            .unwrap();
        assert!(matches!(
            fixture.reserve().await,
            PaymentAttemptDecision::Blocked {
                reason: AllowanceAccountingBlock::InvalidLifecycle
            }
        ));
    }
}

#[tokio::test]
async fn test_accounting_restore_retains_end_before_handoff() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    let backup = crate::export_backup_state(&fixture.storage, path())
        .await
        .unwrap();
    append_end(&fixture, false, false).await;
    assert!(
        crate::backup::restore_backup_state(&fixture.storage, backup)
            .await
            .is_err()
    );
    let result = fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id, checks()))
        .await
        .unwrap();
    assert!(matches!(
        result,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::InvalidLifecycle
        }
    ));
}

#[tokio::test]
async fn test_accounting_restore_with_retained_end_succeeds() {
    let fixture = Fixture::new().await;
    append_end(&fixture, false, false).await;
    let backup = crate::export_backup_state(&fixture.storage, path())
        .await
        .unwrap();
    crate::backup::restore_backup_state(&fixture.storage, backup)
        .await
        .unwrap();
    assert!(fixture.state().requires_reconciliation);
    let record = crate::domain::allowances::allowance_record(
        &fixture.storage,
        &fixture.occurrence.request.counterparty,
        &path(),
        &fixture.allowance,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(record.state, crate::AllowanceLifecycleState::Ended);
}
