use super::super::*;

fn private_list_message_for_app(
    app_id: &str,
    identifier: &str,
    payload: &str,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.private_payment_list".into()),
        app_id: Some(app_id.into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","app_id":"{app_id}","payment_endpoints":{{"{identifier}":"{payload}"}}}}"#
        ),
    }
}

fn constrained_payment_request_message(
    event_id: &str,
    request_id: &str,
    required_app_id: &str,
    accepted_identifier: &str,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        app_id: Some(required_app_id.into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","app_id":"{required_app_id}","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["{accepted_identifier}"],"required_app_id":"{required_app_id}","metadata":{{}}}}}}"#
        ),
    }
}

fn payment_request_cancellation_message(
    event_id: &str,
    request_id: &str,
    app_id: &str,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request_cancellation".into()),
        app_id: Some(app_id.into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request_cancellation","app_id":"{app_id}","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
        ),
    }
}

fn bound_payment_request_message(request_id: &str, payload: &str) -> PrivateApplicationMessage {
    let mut message = constrained_payment_request_message(
        EventId::new_v4().as_str(),
        request_id,
        "server",
        "btc-bitcoin-p2wpkh",
    );
    let mut json: serde_json::Value = serde_json::from_str(&message.raw_json).unwrap();
    json["request"]["payment_endpoints"] = serde_json::json!({"btc-bitcoin-p2wpkh": payload});
    message.raw_json = serde_json::to_string(&json).unwrap();
    message
}

#[tokio::test]
async fn test_bound_requests_keep_destinations_without_private_list_fallback() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                save_authorized_paykit_app(
                    tx,
                    counterparty,
                    paykit_lib::PaykitAppId::new("server").unwrap(),
                    private_app_capabilities(),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let first_id = PaymentRequestId::new_v4();
    let second_id = PaymentRequestId::new_v4();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            bound_payment_request_message(first_id.as_str(), "address-first"),
            bound_payment_request_message(second_id.as_str(), "address-second"),
        ],
        None,
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

    for changed_list in [false, true] {
        if changed_list {
            persist_private_stream_batch(
                &storage,
                counterparty.clone(),
                vec![private_list_message_for_app(
                    "server",
                    "btc-bitcoin-p2wpkh",
                    "unrelated-latest-address",
                )],
                None,
                FixedClock.now(),
            )
            .await
            .unwrap();
        }
        for (id, expected) in [(&first_id, "address-first"), (&second_id, "address-second")] {
            let result = sdk
                .resolve_private_payment_request(counterparty.clone(), id, Some(u64::MAX))
                .await
                .unwrap();
            assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
            assert_eq!(result.private_payment_list_version, None);
            assert_eq!(result.payable_endpoints.len(), 1);
            assert_eq!(result.payable_endpoints[0].endpoint.payload, expected);
            assert_eq!(result.payable_endpoints[0].target.payload, expected);
            let public = sdk
                .resolve_public_payment_request(counterparty.clone(), id)
                .await
                .unwrap();
            assert_eq!(public.status, PublicPaymentResolutionStatus::NoEndpoint);
            assert!(public.payable_endpoints.is_empty());
        }
    }
}

#[tokio::test]
async fn test_bound_request_still_requires_authorized_payee_app() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let request_id = PaymentRequestId::new_v4();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![bound_payment_request_message(
            request_id.as_str(),
            "private-address",
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let result = sdk
        .resolve_private_payment_request(counterparty, &request_id, None)
        .await;
    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_private_request_resolution_rechecks_state_after_authorization_lookup() {
    struct ChangingSessionProvider {
        storage: InMemoryStorage,
        counterparty: PubkyPublicKey,
        request_id: PaymentRequestId,
        recover: bool,
    }

    #[async_trait]
    impl PubkySessionProvider for ChangingSessionProvider {
        async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
            Ok(None)
        }

        async fn clear_session_access(&self) -> Result<()> {
            unreachable!("resolution must not sign out")
        }

        async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
            if self.recover {
                return self
                    .storage
                    .transaction(|tx| {
                        let mut peer = default_linked_peer(self.counterparty.clone());
                        peer.state = LinkedPeerState::RecoveryRequired;
                        tx.save_linked_peer(peer);
                        Ok(None)
                    })
                    .await;
            }
            persist_private_stream_batch(
                &self.storage,
                self.counterparty.clone(),
                vec![payment_request_cancellation_message(
                    EventId::new_v4().as_str(),
                    self.request_id.as_str(),
                    "server",
                )],
                None,
                FixedClock.now(),
            )
            .await?;
            Ok(None)
        }
    }

    for recover in [false, true] {
        let storage = registered_test_storage();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                initialized_at: FixedClock.now(),
            })
            .await
            .unwrap();
        storage
            .transaction(|tx| {
                save_authorized_paykit_app(
                    tx,
                    counterparty.clone(),
                    paykit_lib::PaykitAppId::new("server").unwrap(),
                    private_app_capabilities(),
                );
                Ok(())
            })
            .await
            .unwrap();
        let request_id = PaymentRequestId::new_v4();
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![bound_payment_request_message(
                request_id.as_str(),
                "private-address",
            )],
            None,
            FixedClock.now(),
        )
        .await
        .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            ChangingSessionProvider {
                storage,
                counterparty: counterparty.clone(),
                request_id: request_id.clone(),
                recover,
            },
            TestPaymentAdapter,
            PaykitSdkConfig::new("test-app").unwrap(),
            FixedClock,
        );

        let result = sdk
            .resolve_private_payment_request(counterparty, &request_id, None)
            .await;
        if recover {
            let resolution = result.unwrap();
            assert_eq!(
                resolution.state,
                PrivatePaymentResolutionState::RecoveryPending
            );
            assert!(resolution.payable_endpoints.is_empty());
        } else {
            assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        }
    }
}

#[tokio::test]
async fn test_resolve_private_payment_request_enforces_request_constraints() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let request_id = "550e8400-e29b-41d4-a716-446655440000";
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            constrained_payment_request_message(
                "650e8400-e29b-41d4-a716-446655440000",
                request_id,
                "server",
                "btc-lightning-bolt11",
            ),
            private_list_message_for_app("bitkit", "btc-lightning-bolt11", "bitkit-lightning"),
        ],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                save_authorized_paykit_app(
                    tx,
                    counterparty.clone(),
                    paykit_lib::PaykitAppId::new("bitkit").unwrap(),
                    private_app_capabilities(),
                );
                save_authorized_paykit_app(
                    tx,
                    counterparty,
                    paykit_lib::PaykitAppId::new("server").unwrap(),
                    private_app_capabilities(),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("server").unwrap(),
        FixedClock,
    );

    let missing = sdk
        .resolve_private_payment_request(
            counterparty.clone(),
            &PaymentRequestId::new(request_id).unwrap(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(missing.status, PrivatePaymentResolutionStatus::NoEndpoint);
    assert_eq!(
        missing.state,
        PrivatePaymentResolutionState::NoPrivateEndpoint
    );
    assert_eq!(missing.private_payment_list_version, None);
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_list_message_for_app(
            "server",
            "btc-lightning-bolt11",
            "server-lightning",
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let result = sdk
        .resolve_private_payment_request(
            counterparty,
            &PaymentRequestId::new(request_id).unwrap(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.payable_endpoints.len(), 1);
    assert_eq!(
        result.payable_endpoints[0].endpoint.app_id.as_str(),
        "server"
    );
    assert_eq!(
        result.payable_endpoints[0].endpoint.identifier,
        "btc-lightning-bolt11"
    );
    assert_eq!(
        result.payable_endpoints[0].endpoint.payload,
        "server-lightning"
    );
}

#[tokio::test]
async fn test_resolve_private_payment_request_rejects_terminal_request() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let request_id = "550e8400-e29b-41d4-a716-446655440000";
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            constrained_payment_request_message(
                "650e8400-e29b-41d4-a716-446655440000",
                request_id,
                "server",
                "btc-lightning-bolt11",
            ),
            payment_request_cancellation_message(
                "750e8400-e29b-41d4-a716-446655440000",
                request_id,
                "server",
            ),
        ],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_payment_request(
            counterparty,
            &PaymentRequestId::new(request_id).unwrap(),
            None,
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_resolve_private_payment_request_rejects_other_target_app() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let request_id = PaymentRequestId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![constrained_payment_request_message(
            "650e8400-e29b-41d4-a716-446655440000",
            request_id.as_str(),
            "server",
            "btc-lightning-bolt11",
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    enqueue_payment_request_response_message(
        &storage,
        counterparty.clone(),
        &paykit_lib::PaykitAppId::new("other-app").unwrap(),
        &PaymentRequestEvent::Acceptance(PaymentRequestAcceptance::new(
            EventId::new_v4(),
            request_id.clone(),
        )),
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_payment_request(counterparty, &request_id, None)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}
