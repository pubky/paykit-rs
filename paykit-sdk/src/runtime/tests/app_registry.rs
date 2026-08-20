use super::*;
use crate::runtime::app_removal::{
    begin_paykit_app_removal, reactivate_paykit_app, require_app_capability_downgrade_safe,
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
                ));
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
    let error = require_app_capability_downgrade_safe(
        &storage,
        &app_id(),
        previous,
        next,
        FixedClock.now(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Policy { .. }));
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
