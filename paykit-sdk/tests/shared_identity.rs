use async_trait::async_trait;
use chrono::Utc;
use paykit_sdk::storage::{PrivateStreamItemRecord, PublicEndpointRecord, StorageState};
use paykit_sdk::{
    load_public_endpoint_records, IdentityState, InMemoryStorage, PaykitSdk, PaykitSdkConfig,
    PaymentAdapter, PrivateStreamParseStatus, PubkyPublicKey, PubkySessionAccess,
    PubkySessionProvider, PublicationStatus, Result, StorageAdapter,
};
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct NoSessionProvider;

#[async_trait]
impl PubkySessionProvider for NoSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        Ok(None)
    }

    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        Ok(None)
    }

    async fn clear_session_access(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_two_apps_read_one_aggregated_private_state() {
    let identity = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let now = Utc::now();
    let mut state = StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(identity),
            initialized_at: now,
        }),
        next_private_stream_item_id: 3,
        next_receive_batch_id: 2,
        authorized_paykit_apps: HashMap::from([(
            counterparty.clone(),
            HashMap::from([
                (
                    paykit_lib::PaykitAppId::new("bitkit").unwrap(),
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: true,
                        payment_requests: false,
                        receipts: false,
                        outgoing_payments: false,
                    },
                ),
                (
                    paykit_lib::PaykitAppId::new("paykit-server").unwrap(),
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: true,
                        payment_requests: false,
                        receipts: false,
                        outgoing_payments: false,
                    },
                ),
            ]),
        )]),
        ..StorageState::default()
    };
    for (stream_item_id, app_id, identifier, payload) in [
        (1, "bitkit", "btc-lightning-bolt11", "ln-private"),
        (2, "paykit-server", "usdt-tron", "usdt-private"),
    ] {
        state.private_stream_items.push(PrivateStreamItemRecord {
            stream_item_id,
            counterparty: counterparty.clone(),
            receive_batch_id: 1,
            raw_json: format!(
                r#"{{"version":1,"kind":"paykit.private_payment_list","app_id":"{app_id}","payment_endpoints":{{"{identifier}":"{payload}"}}}}"#
            ),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.private_payment_list".into()),
            parsed_app_id: Some(app_id.into()),
            known_paykit_kind: Some("paykit.private_payment_list".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: now,
        });
    }
    let storage = InMemoryStorage::from_state(state);
    let bitkit = PaykitSdk::new(
        storage.clone(),
        NoSessionProvider,
        NoPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    let server = PaykitSdk::new(
        storage,
        NoSessionProvider,
        NoPaymentAdapter,
        PaykitSdkConfig::new("paykit-server").unwrap(),
    );

    let bitkit_views = bitkit
        .current_private_payment_lists(&counterparty)
        .await
        .unwrap();
    let server_views = server
        .current_private_payment_lists(&counterparty)
        .await
        .unwrap();

    assert_eq!(bitkit_views, server_views);
    assert_eq!(bitkit_views.len(), 2);
    assert!(bitkit_views.iter().any(|view| {
        view.app_id.as_str() == "bitkit"
            && view
                .payment_endpoints
                .get("btc-lightning-bolt11")
                .map(String::as_str)
                == Some("ln-private")
    }));
    assert!(bitkit_views.iter().any(|view| {
        view.app_id.as_str() == "paykit-server"
            && view.payment_endpoints.get("usdt-tron").map(String::as_str) == Some("usdt-private")
    }));
}

#[derive(Clone, Copy)]
struct NoPaymentAdapter;

#[async_trait]
impl PaymentAdapter for NoPaymentAdapter {}

#[tokio::test]
async fn test_sign_out_preserves_shared_app_state() {
    let storage = InMemoryStorage::new();
    let identity = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let now = Utc::now();
    storage
        .transaction({
            let identity = identity.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(identity),
                    initialized_at: now,
                });
                for app_id in ["bitkit", "paykit-server"] {
                    tx.save_public_endpoint_record(PublicEndpointRecord {
                        app_id: paykit_sdk::PaykitAppId::new(app_id).unwrap(),
                        identifier: "btc-lightning-bolt11".into(),
                        payload: Some(format!("{app_id}-endpoint")),
                        status: PublicationStatus::Published,
                        updated_at: now,
                        last_error: None,
                    });
                }
                Ok(())
            }
        })
        .await
        .unwrap();

    let sdk = PaykitSdk::new(
        storage.clone(),
        NoSessionProvider,
        NoPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
    );

    let status = sdk.sign_out().await.unwrap();

    assert_eq!(
        status.capability,
        paykit_sdk::PubkyIdentityCapability::SignedOut
    );
    assert_eq!(status.public_key.as_ref(), Some(&identity));
    let records = load_public_endpoint_records(&storage).await.unwrap();
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .any(|record| record.app_id.as_str() == "bitkit"));
    assert!(records
        .iter()
        .any(|record| record.app_id.as_str() == "paykit-server"));
}
