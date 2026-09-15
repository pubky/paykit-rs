use super::*;

fn app_id() -> PaykitAppId {
    PaykitAppId::new("bitkit").unwrap()
}

fn app_capabilities() -> PaykitAppCapabilities {
    PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    }
}

#[tokio::test]
async fn test_app_registry_round_trips_through_public_storage() {
    let setup = TestSetup::new().await;
    let mut registry = PaykitAppRegistry::new(Some(Keypair::random().public_key()));
    registry
        .register_app(
            app_id(),
            PaykitApp::new("Bitkit", app_capabilities()).unwrap(),
        )
        .unwrap();

    create_paykit_app_registry(&setup.session, &registry)
        .await
        .unwrap();
    let fetched = get_paykit_app_registry(&setup.public_storage, &setup.public_key)
        .await
        .unwrap();
    let versioned = get_paykit_app_registry_with_etag(&setup.public_storage, &setup.public_key)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(fetched, Some(registry));
    assert_eq!(versioned.0, fetched.unwrap());
    assert!(!versioned.1.is_empty());
    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_app_registry_rejects_stale_etag_update() {
    let setup = TestSetup::new().await;
    let mut registry = PaykitAppRegistry::new(Some(Keypair::random().public_key()));
    registry
        .register_app(
            app_id(),
            PaykitApp::new("Bitkit", app_capabilities()).unwrap(),
        )
        .unwrap();
    create_paykit_app_registry(&setup.session, &registry)
        .await
        .unwrap();

    let (mut first_update, etag) =
        get_paykit_app_registry_with_etag(&setup.public_storage, &setup.public_key)
            .await
            .unwrap()
            .unwrap();
    let mut stale_update = first_update.clone();
    first_update
        .register_app(
            PaykitAppId::new("paykit-server").unwrap(),
            PaykitApp::new("Paykit Server", app_capabilities()).unwrap(),
        )
        .unwrap();
    stale_update
        .register_app(
            PaykitAppId::new("exchange").unwrap(),
            PaykitApp::new("Exchange", app_capabilities()).unwrap(),
        )
        .unwrap();

    update_paykit_app_registry(&setup.session, &first_update, &etag)
        .await
        .unwrap();
    let error = update_paykit_app_registry(&setup.session, &stale_update, &etag)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PaykitError::Transport { source, .. }
            if matches!(
                source.downcast_ref::<pubky::Error>(),
                Some(pubky::Error::Request(pubky::errors::RequestError::Server {
                    status,
                    ..
                })) if *status == pubky::StatusCode::PRECONDITION_FAILED
            )
    ));

    let stored = get_paykit_app_registry(&setup.public_storage, &setup.public_key)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.apps().contains_key(&app_id()));
    assert!(stored
        .apps()
        .contains_key(&PaykitAppId::new("paykit-server").unwrap()));
    assert!(!stored
        .apps()
        .contains_key(&PaykitAppId::new("exchange").unwrap()));
    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn endpoint_round_trip_and_update() {
    let setup = TestSetup::new().await;

    let method = PaymentEndpointIdentifier::new("onchain").unwrap();
    let endpoint = PaymentEndpointPayload::new("{\"address\":\"bc1...\"}");

    set_payment_endpoint(&setup.session, &app_id(), method.clone(), endpoint.clone())
        .await
        .unwrap();

    let fetched =
        get_payment_endpoint(&setup.public_storage, &setup.public_key, &app_id(), &method)
            .await
            .unwrap();
    assert_eq!(fetched, Some(endpoint.clone()));

    let list = get_payment_list(&setup.public_storage, &setup.public_key, &app_id())
        .await
        .unwrap();
    assert_eq!(
        list,
        PaymentList {
            payment_endpoints: vec![(method.clone(), endpoint.clone())]
                .into_iter()
                .collect()
        }
    );

    let new_endpoint = PaymentEndpointPayload::new("{\"address\":\"1c1...\"}");

    set_payment_endpoint(
        &setup.session,
        &app_id(),
        method.clone(),
        new_endpoint.clone(),
    )
    .await
    .unwrap();

    let updated =
        get_payment_endpoint(&setup.public_storage, &setup.public_key, &app_id(), &method)
            .await
            .unwrap();
    assert_eq!(updated, Some(new_endpoint.clone()));

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_endpoint_conditional_updates_reject_stale_revisions() {
    let setup = TestSetup::new().await;
    let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let initial = PaymentEndpointPayload::new("ln-initial");
    let updated = PaymentEndpointPayload::new("ln-updated");

    create_payment_endpoint(
        &setup.session,
        &app_id(),
        identifier.clone(),
        initial.clone(),
    )
    .await
    .unwrap();
    let (payload, initial_revision) = get_payment_endpoint_with_revision(
        &setup.public_storage,
        &setup.public_key,
        &app_id(),
        &identifier,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(payload, Some(initial));

    update_payment_endpoint(
        &setup.session,
        &app_id(),
        identifier.clone(),
        updated.clone(),
        &initial_revision,
    )
    .await
    .unwrap();
    let stale_delete = remove_payment_endpoint_if_revision(
        &setup.session,
        &app_id(),
        identifier.clone(),
        &initial_revision,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        stale_delete,
        PaykitError::Transport { source, .. }
            if matches!(
                source.downcast_ref::<pubky::Error>(),
                Some(pubky::Error::Request(pubky::errors::RequestError::Server {
                    status,
                    ..
                })) if *status == pubky::StatusCode::PRECONDITION_FAILED
            )
    ));

    let (payload, current_revision) = get_payment_endpoint_with_revision(
        &setup.public_storage,
        &setup.public_key,
        &app_id(),
        &identifier,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(payload, Some(updated));
    remove_payment_endpoint_if_revision(
        &setup.session,
        &app_id(),
        identifier.clone(),
        &current_revision,
    )
    .await
    .unwrap();
    assert!(get_payment_endpoint_with_revision(
        &setup.public_storage,
        &setup.public_key,
        &app_id(),
        &identifier,
    )
    .await
    .unwrap()
    .is_none());

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn missing_endpoint_returns_none() {
    let setup = TestSetup::new().await;
    let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();

    let missing =
        get_payment_endpoint(&setup.public_storage, &setup.public_key, &app_id(), &method)
            .await
            .unwrap();
    assert!(missing.is_none());

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn list_reflects_additions_and_removals() {
    let setup = TestSetup::new().await;

    let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
    let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let onchain_data = PaymentEndpointPayload::new("bc1p...");
    let lightning_data = PaymentEndpointPayload::new("ln...");

    set_payment_endpoint(
        &setup.session,
        &app_id(),
        onchain.clone(),
        onchain_data.clone(),
    )
    .await
    .unwrap();
    set_payment_endpoint(
        &setup.session,
        &app_id(),
        lightning.clone(),
        lightning_data.clone(),
    )
    .await
    .unwrap();

    let list = get_payment_list(&setup.public_storage, &setup.public_key, &app_id())
        .await
        .unwrap();
    let mut expected = HashMap::new();
    expected.insert(onchain.clone(), onchain_data.clone());
    expected.insert(lightning.clone(), lightning_data.clone());
    assert_eq!(list.payment_endpoints, expected);

    remove_payment_endpoint(&setup.session, &app_id(), onchain.clone())
        .await
        .unwrap();
    let list = get_payment_list(&setup.public_storage, &setup.public_key, &app_id())
        .await
        .unwrap();
    assert_eq!(
        list.payment_endpoints,
        vec![(lightning.clone(), lightning_data.clone())]
            .into_iter()
            .collect()
    );

    remove_payment_endpoint(&setup.session, &app_id(), lightning.clone())
        .await
        .unwrap();
    let empty = get_payment_list(&setup.public_storage, &setup.public_key, &app_id())
        .await
        .unwrap();
    assert!(empty.payment_endpoints.is_empty());

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn list_fetches_multiple_pages() {
    let setup = TestSetup::new().await;
    let mut expected = HashMap::new();

    for index in 0..105 {
        let identifier = PaymentEndpointIdentifier::new(format!("endpoint-{index:03}")).unwrap();
        let payload = PaymentEndpointPayload::new(format!("payload-{index:03}"));
        set_payment_endpoint(
            &setup.session,
            &app_id(),
            identifier.clone(),
            payload.clone(),
        )
        .await
        .unwrap();
        expected.insert(identifier, payload);
    }

    let list = get_payment_list(&setup.public_storage, &setup.public_key, &app_id())
        .await
        .unwrap();
    assert_eq!(list.payment_endpoints, expected);

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_payment_list_fetch_limits_endpoint_count_and_payload_bytes() {
    let setup = TestSetup::new().await;
    for (identifier, payload) in [("endpoint-1", "payload-1"), ("endpoint-2", "payload-2")] {
        set_payment_endpoint(
            &setup.session,
            &app_id(),
            PaymentEndpointIdentifier::new(identifier).unwrap(),
            PaymentEndpointPayload::new(payload),
        )
        .await
        .unwrap();
    }

    let list =
        get_payment_list_with_limits(&setup.public_storage, &setup.public_key, &app_id(), 2, 18)
            .await
            .unwrap();
    assert_eq!(list.payment_endpoints.len(), 2);

    let endpoint_limit =
        get_payment_list_with_limits(&setup.public_storage, &setup.public_key, &app_id(), 1, 18)
            .await;
    let endpoint_limit = endpoint_limit.unwrap_err();
    assert!(matches!(endpoint_limit, PaykitError::InvalidData { .. }));
    assert!(is_payment_list_limit_exceeded(&endpoint_limit));

    let payload_limit =
        get_payment_list_with_limits(&setup.public_storage, &setup.public_key, &app_id(), 2, 8)
            .await;
    let payload_limit = payload_limit.unwrap_err();
    assert!(matches!(payload_limit, PaykitError::InvalidData { .. }));
    assert!(is_payment_list_limit_exceeded(&payload_limit));

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn removing_missing_endpoint_is_idempotent() {
    let setup = TestSetup::new().await;
    let method = PaymentEndpointIdentifier::new("unused").unwrap();

    remove_payment_endpoint(&setup.session, &app_id(), method)
        .await
        .expect("removing non-existent endpoint should be idempotent");

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_invalid_utf8_endpoint_returns_invalid_data() {
    let setup = TestSetup::new().await;
    let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let path = crate::pubky_routing::payment_endpoint_path(&app_id(), &identifier);

    setup
        .session
        .storage()
        .put(path, vec![0xff])
        .await
        .expect("invalid UTF-8 fixture should be stored");

    let result = get_payment_endpoint(
        &setup.public_storage,
        &setup.public_key,
        &app_id(),
        &identifier,
    )
    .await;
    assert!(matches!(result, Err(PaykitError::InvalidData { .. })));

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_invalid_payment_endpoint_listing_entry_returns_invalid_data() {
    let setup = TestSetup::new().await;
    let invalid_identifier = "a".repeat(65);
    let path = format!(
        "{}{invalid_identifier}",
        crate::pubky_routing::payment_endpoint_path_prefix(&app_id())
    );

    setup
        .session
        .storage()
        .put(path, "non-empty payload".to_string())
        .await
        .expect("invalid listing entry fixture should be stored");

    let result = get_payment_list(&setup.public_storage, &setup.public_key, &app_id())
        .await
        .unwrap_err();
    assert!(matches!(result, PaykitError::InvalidData { .. }));
    assert!(!is_payment_list_limit_exceeded(&result));

    setup.raw_session.signout().await.unwrap();
}
