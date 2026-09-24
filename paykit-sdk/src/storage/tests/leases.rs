use super::*;

#[tokio::test]
async fn test_peer_link_operation_lease_blocks_until_released() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
    let blocked = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap();
    assert!(blocked.is_none());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.release_peer_link_operation(&counterparty, first.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();
    let second = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap();

    assert!(second.is_some());
}

#[tokio::test]
async fn test_peer_link_operation_lease_can_be_reclaimed_after_expiry() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
    let second = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();

    assert_ne!(first.lease_id, second.lease_id);
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_peer_link_operation_stale_release_keeps_newer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
    let second = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.release_peer_link_operation(&counterparty, first.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_paykit_app_operation_lease_blocks_same_app_only() {
    let storage = InMemoryStorage::new();
    let bitkit = app_id();
    let server = paykit_lib::PaykitAppId::new("paykit-server").unwrap();

    let first = storage
        .transaction({
            let bitkit = bitkit.clone();
            move |tx| {
                tx.claim_paykit_app_operation(
                    &bitkit,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
    let (same_app, other_app) = storage
        .transaction({
            let bitkit = bitkit.clone();
            let server = server.clone();
            move |tx| {
                Ok((
                    tx.claim_paykit_app_operation(
                        &bitkit,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    )?,
                    tx.claim_paykit_app_operation(
                        &server,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    )?,
                ))
            }
        })
        .await
        .unwrap();

    assert!(same_app.is_none());
    assert!(other_app.is_some());
    assert_eq!(first.app_id, bitkit);
}

#[tokio::test]
async fn test_paykit_app_operation_stale_release_keeps_newer_lease() {
    let storage = InMemoryStorage::new();
    let app_id = app_id();

    let first = storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                tx.claim_paykit_app_operation(
                    &app_id,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
    let second = storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                tx.claim_paykit_app_operation(
                    &app_id,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();

    storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                tx.release_paykit_app_operation(&app_id, first.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        storage
            .transaction(move |tx| Ok(tx.paykit_app_operation_lease(&app_id)))
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_stale_peer_link_lease_cannot_overwrite_outbound_status() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let (record, first_lease) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let record = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ))?;
                let lease = tx
                    .claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    )?
                    .unwrap();
                Ok((record, lease))
            }
        })
        .await
        .unwrap();
    let active_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    )?
                    .unwrap())
            }
        })
        .await
        .unwrap();
    let sent = mark_outbound_sent(record.clone(), timestamp() + chrono::Duration::seconds(12));
    storage
        .transaction({
            let sent = sent.clone();
            let active_lease = active_lease.clone();
            move |tx| {
                require_peer_link_operation_lease(tx, &active_lease)?;
                tx.save_outbound_private_message(sent)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let failed = mark_outbound_failed(
        record,
        "late failed send".into(),
        timestamp() + chrono::Duration::seconds(13),
    );
    let stale_result: Result<()> = storage
        .transaction({
            let failed = failed.clone();
            move |tx| {
                require_peer_link_operation_lease(tx, &first_lease)?;
                tx.save_outbound_private_message(failed)?;
                Ok(())
            }
        })
        .await;

    assert!(matches!(stale_result, Err(PaykitSdkError::Policy { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::Sent
    );
    assert!(snapshot.outbound_private_messages[0].last_error.is_none());
}
