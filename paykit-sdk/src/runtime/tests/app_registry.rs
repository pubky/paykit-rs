use super::*;
use crate::runtime::app_removal::{
    begin_paykit_app_removal, reactivate_paykit_app, require_app_capability_downgrade_safe,
    restore_app_capabilities, stage_app_capability_update,
};

fn capabilities() -> paykit_lib::PaykitAppCapabilities {
    paykit_lib::PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    }
}

#[tokio::test]
async fn test_aborted_app_removal_restores_previous_active_state() {
    let storage = registered_test_storage();

    let was_active = begin_paykit_app_removal(&storage, &app_id()).await.unwrap();
    assert!(was_active);
    storage
        .transaction(|tx| {
            assert!(!tx.paykit_app_is_registered(&app_id()));
            assert!(tx.paykit_app_is_retired(&app_id()));
            Ok(())
        })
        .await
        .unwrap();

    reactivate_paykit_app(&storage, app_id()).await.unwrap();
    storage
        .transaction(|tx| {
            assert!(tx.paykit_app_is_registered(&app_id()));
            assert!(!tx.paykit_app_is_retired(&app_id()));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_app_removal_does_not_reactivate_previously_retired_app() {
    let storage = registered_test_storage();
    storage
        .transaction(|tx| {
            tx.retire_paykit_app(app_id());
            Ok(())
        })
        .await
        .unwrap();

    let was_active = begin_paykit_app_removal(&storage, &app_id()).await.unwrap();

    assert!(!was_active);
    storage
        .transaction(|tx| {
            assert!(!tx.paykit_app_is_registered(&app_id()));
            assert!(tx.paykit_app_is_retired(&app_id()));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_capability_downgrade_blocks_only_owned_active_work() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"btc":"address"}}"#.into(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let previous = capabilities();
    let mut next = previous;
    next.receipts = false;
    require_app_capability_downgrade_safe(&storage, &app_id(), previous, next, FixedClock.now())
        .await
        .unwrap();

    next = previous;
    next.private_payments = false;
    let error = stage_app_capability_update(&storage, &app_id(), None, next, FixedClock.now())
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Policy { .. }));
    storage
        .transaction(|tx| {
            assert_eq!(tx.paykit_app_capabilities(&app_id()), Some(previous));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_outgoing_payment_metadata_can_change_with_active_work() {
    let storage = registered_test_storage();
    let previous = capabilities();
    let mut next = previous;
    next.outgoing_payments = false;

    require_app_capability_downgrade_safe(&storage, &app_id(), previous, next, FixedClock.now())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_staged_capability_downgrade_blocks_new_work_until_restored() {
    let storage = registered_test_storage();
    let previous = capabilities();
    let mut next = previous;
    next.private_payments = false;

    let staged =
        stage_app_capability_update(&storage, &app_id(), Some(previous), next, FixedClock.now())
            .await
            .unwrap();
    assert_eq!(staged, Some((previous, next)));
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let enqueue = crate::domain::outbound_private::enqueue_private_message(
        &storage,
        counterparty,
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
            .into(),
        FixedClock.now(),
    )
    .await;
    assert!(matches!(enqueue, Err(PaykitSdkError::Policy { .. })));

    restore_app_capabilities(&storage, &app_id(), next, previous)
        .await
        .unwrap();
    storage
        .transaction(|tx| {
            assert_eq!(tx.paykit_app_capabilities(&app_id()), Some(previous));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_staged_capability_upgrade_stays_blocked_until_publish_commits() {
    let storage = registered_test_storage();
    let mut previous = capabilities();
    previous.private_payments = false;
    storage
        .transaction(move |tx| {
            tx.save_paykit_app_capabilities(&app_id(), previous);
            Ok(())
        })
        .await
        .unwrap();
    let mut next = previous;
    next.private_payments = true;

    let staged =
        stage_app_capability_update(&storage, &app_id(), Some(previous), next, FixedClock.now())
            .await
            .unwrap();
    assert_eq!(staged, Some((previous, previous)));

    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let enqueue = crate::domain::outbound_private::enqueue_private_message(
        &storage,
        counterparty,
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
            .into(),
        FixedClock.now(),
    )
    .await;
    assert!(matches!(enqueue, Err(PaykitSdkError::Policy { .. })));

    restore_app_capabilities(&storage, &app_id(), previous, previous)
        .await
        .unwrap();
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_capability_rollback_preserves_newer_update() {
    let storage = registered_test_storage();
    let previous = capabilities();
    let mut staged = previous;
    staged.outgoing_payments = false;
    let mut newer = staged;
    newer.receipts = false;
    storage
        .transaction(move |tx| {
            tx.save_paykit_app_capabilities(&app_id(), newer);
            Ok(())
        })
        .await
        .unwrap();

    restore_app_capabilities(&storage, &app_id(), staged, previous)
        .await
        .unwrap();

    storage
        .transaction(|tx| {
            assert_eq!(tx.paykit_app_capabilities(&app_id()), Some(newer));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_remote_app_republish_does_not_stage_unregistered_capabilities() {
    let storage = InMemoryStorage::new();
    let previous = capabilities();

    let staged = stage_app_capability_update(
        &storage,
        &app_id(),
        Some(previous),
        previous,
        FixedClock.now(),
    )
    .await
    .unwrap();

    assert_eq!(staged, None);
    storage
        .transaction(|tx| {
            assert!(!tx.paykit_app_is_registered(&app_id()));
            assert!(!tx.paykit_app_is_retired(&app_id()));
            assert_eq!(tx.paykit_app_capabilities(&app_id()), None);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_restored_retired_app_can_stage_republish_without_capability_cache() {
    let storage = InMemoryStorage::new();
    storage
        .transaction(|tx| {
            tx.retire_paykit_app(app_id());
            Ok(())
        })
        .await
        .unwrap();

    let staged = stage_app_capability_update(
        &storage,
        &app_id(),
        Some(capabilities()),
        capabilities(),
        FixedClock.now(),
    )
    .await
    .unwrap();

    assert_eq!(staged, None);
    storage
        .transaction(|tx| {
            assert!(tx.paykit_app_is_retired(&app_id()));
            assert_eq!(tx.paykit_app_capabilities(&app_id()), None);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_remove_unknown_app_can_stage_later_publish() {
    let storage = InMemoryStorage::new();
    assert!(!begin_paykit_app_removal(&storage, &app_id()).await.unwrap());

    let staged =
        stage_app_capability_update(&storage, &app_id(), None, capabilities(), FixedClock.now())
            .await
            .unwrap();

    assert_eq!(staged, None);
}
