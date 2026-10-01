use super::super::*;

#[tokio::test]
async fn test_resolve_private_candidate_batch_preserves_private_state() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let endpoint = private_endpoint_candidate("ln-private");
    let result = sdk
        .resolve_private_candidate_batch(
            endpoint.counterparty.clone(),
            None,
            vec![endpoint],
            PrivatePaymentResolutionState::RecoveryPending,
            7,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.state, PrivatePaymentResolutionState::RecoveryPending);
    assert_eq!(result.private_payment_list_version, Some(7));
    assert_eq!(result.payable_endpoints.len(), 1);
}

#[tokio::test]
async fn test_resolve_public_candidate_batch_returns_ordered_payable_endpoints() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let first = public_endpoint_candidate("ln-first");
    let mut second = first.clone();
    second.payload = "ln-second".into();

    let result = sdk
        .resolve_public_candidate_batch(
            first.counterparty.clone(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            vec![first.clone(), second.clone()],
        )
        .await
        .unwrap();

    assert_eq!(result.status, PublicPaymentResolutionStatus::Payable);
    assert_eq!(result.payable_endpoints.len(), 2);
    assert_eq!(result.payable_endpoints[0].endpoint, first);
    assert_eq!(result.payable_endpoints[0].target.payload, "ln-first");
    assert_eq!(result.payable_endpoints[1].endpoint, second);
    assert_eq!(result.payable_endpoints[1].target.payload, "ln-second");
}
