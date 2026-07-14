use super::*;

fn marker_private_capabilities() -> PaykitReceiverCapabilities {
    PaykitReceiverCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: false,
    }
}

fn marker_public_capabilities() -> PaykitReceiverCapabilities {
    PaykitReceiverCapabilities {
        private_payments: false,
        payment_requests: false,
        receipts: false,
        outgoing_payments: true,
    }
}

#[tokio::test]
async fn test_sync_public_endpoints_requires_pubky_session() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.sync_public_endpoints().await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_sync_public_endpoints_rejects_reentrant_call() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage,
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("test operation").unwrap();

    let result = sdk.sync_public_endpoints().await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
}

#[test]
fn test_receiver_marker_capabilities_require_private_identity_for_private_workflows() {
    assert!(matches!(
        crate::runtime::public_endpoints::validate_receiver_marker_capabilities(
            &marker_private_capabilities(),
            PubkyIdentityCapability::PublicOnly
        ),
        Err(PaykitSdkError::Policy(_))
    ));
    assert!(
        crate::runtime::public_endpoints::validate_receiver_marker_capabilities(
            &marker_private_capabilities(),
            PubkyIdentityCapability::PrivateLinkCapable
        )
        .is_ok()
    );
    assert!(
        crate::runtime::public_endpoints::validate_receiver_marker_capabilities(
            &marker_public_capabilities(),
            PubkyIdentityCapability::PublicOnly
        )
        .is_ok()
    );
}

#[tokio::test]
async fn test_publish_receiver_marker_rejects_private_capabilities_without_local_secret() {
    let storage = InMemoryStorage::new();
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            capability: PubkyIdentityCapability::PublicOnly,
            local_secret_available: false,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .publish_paykit_receiver_marker(marker_private_capabilities())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
}
