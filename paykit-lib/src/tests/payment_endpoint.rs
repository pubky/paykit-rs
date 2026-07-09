use super::*;

fn server_receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/server").unwrap()
}

fn receiver_capabilities() -> PaykitReceiverCapabilities {
    PaykitReceiverCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: false,
    }
}

#[tokio::test]
async fn endpoint_round_trip_and_update() {
    let setup = TestSetup::new().await;

    let method = PaymentEndpointIdentifier::new("onchain").unwrap();
    let endpoint = PaymentEndpointPayload::new("{\"address\":\"bc1...\"}");

    set_payment_endpoint(
        &setup.session,
        &receiver_path(),
        method.clone(),
        endpoint.clone(),
    )
    .await
    .unwrap();

    let fetched = get_payment_endpoint(
        &setup.public_storage,
        &setup.public_key,
        &receiver_path(),
        &method,
    )
    .await
    .unwrap();
    assert_eq!(fetched, Some(endpoint.clone()));

    let list = get_payment_list(&setup.public_storage, &setup.public_key, &receiver_path())
        .await
        .unwrap();
    let receiver_paths = list_paykit_receiver_paths(&setup.public_storage, &setup.public_key)
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
    assert!(receiver_paths.contains(&receiver_path()));

    let new_endpoint = PaymentEndpointPayload::new("{\"address\":\"1c1...\"}");

    set_payment_endpoint(
        &setup.session,
        &receiver_path(),
        method.clone(),
        new_endpoint.clone(),
    )
    .await
    .unwrap();

    let updated = get_payment_endpoint(
        &setup.public_storage,
        &setup.public_key,
        &receiver_path(),
        &method,
    )
    .await
    .unwrap();
    assert_eq!(updated, Some(new_endpoint.clone()));

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn receiver_marker_round_trip_and_discovery() {
    let setup = TestSetup::new().await;
    let marker = PaykitReceiverMarker::new(server_receiver_path(), receiver_capabilities());

    publish_paykit_receiver_marker(&setup.session, &marker)
        .await
        .unwrap();

    let fetched = get_paykit_receiver_marker(
        &setup.public_storage,
        &setup.public_key,
        &server_receiver_path(),
    )
    .await
    .unwrap();
    let receiver_paths = list_paykit_receiver_paths(&setup.public_storage, &setup.public_key)
        .await
        .unwrap();
    let payment_list = get_payment_list(
        &setup.public_storage,
        &setup.public_key,
        &server_receiver_path(),
    )
    .await
    .unwrap();

    assert_eq!(fetched, Some(marker));
    assert!(receiver_paths.contains(&server_receiver_path()));
    assert!(payment_list.payment_endpoints.is_empty());

    remove_paykit_receiver_marker(&setup.session, &server_receiver_path())
        .await
        .unwrap();
    let removed = get_paykit_receiver_marker(
        &setup.public_storage,
        &setup.public_key,
        &server_receiver_path(),
    )
    .await
    .unwrap();
    let receiver_paths = list_paykit_receiver_paths(&setup.public_storage, &setup.public_key)
        .await
        .unwrap();
    assert!(removed.is_none());
    assert!(!receiver_paths.contains(&server_receiver_path()));

    remove_paykit_receiver_marker(&setup.session, &server_receiver_path())
        .await
        .unwrap();

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn invalid_receiver_marker_does_not_block_sibling_discovery() {
    let setup = TestSetup::new().await;
    let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let endpoint = PaymentEndpointPayload::new("lnbc...");

    set_payment_endpoint(
        &setup.session,
        &receiver_path(),
        method.clone(),
        endpoint.clone(),
    )
    .await
    .unwrap();
    setup
        .session
        .storage()
        .put(
            format!(
                "{PAYKIT_PATH_PREFIX}/{}/receiver.json",
                server_receiver_path()
            ),
            "not-json".to_string(),
        )
        .await
        .unwrap();

    let direct_marker = get_paykit_receiver_marker(
        &setup.public_storage,
        &setup.public_key,
        &server_receiver_path(),
    )
    .await;
    let receiver_paths = list_paykit_receiver_paths(&setup.public_storage, &setup.public_key)
        .await
        .unwrap();

    assert!(matches!(
        direct_marker,
        Err(PaykitError::InvalidData { .. })
    ));
    assert!(receiver_paths.contains(&receiver_path()));
    assert!(!receiver_paths.contains(&server_receiver_path()));

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn empty_receiver_marker_is_invalid_and_does_not_advertise_receiver() {
    let setup = TestSetup::new().await;

    setup
        .session
        .storage()
        .put(
            format!(
                "{PAYKIT_PATH_PREFIX}/{}/receiver.json",
                server_receiver_path()
            ),
            String::new(),
        )
        .await
        .unwrap();

    let direct_marker = get_paykit_receiver_marker(
        &setup.public_storage,
        &setup.public_key,
        &server_receiver_path(),
    )
    .await;
    let receiver_paths = list_paykit_receiver_paths(&setup.public_storage, &setup.public_key)
        .await
        .unwrap();

    assert!(matches!(
        direct_marker,
        Err(PaykitError::InvalidData { .. })
    ));
    assert!(!receiver_paths.contains(&server_receiver_path()));

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn missing_endpoint_returns_none() {
    let setup = TestSetup::new().await;
    let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();

    let missing = get_payment_endpoint(
        &setup.public_storage,
        &setup.public_key,
        &receiver_path(),
        &method,
    )
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
        &receiver_path(),
        onchain.clone(),
        onchain_data.clone(),
    )
    .await
    .unwrap();
    set_payment_endpoint(
        &setup.session,
        &receiver_path(),
        lightning.clone(),
        lightning_data.clone(),
    )
    .await
    .unwrap();

    let list = get_payment_list(&setup.public_storage, &setup.public_key, &receiver_path())
        .await
        .unwrap();
    let mut expected = HashMap::new();
    expected.insert(onchain.clone(), onchain_data.clone());
    expected.insert(lightning.clone(), lightning_data.clone());
    assert_eq!(list.payment_endpoints, expected);

    remove_payment_endpoint(&setup.session, &receiver_path(), onchain.clone())
        .await
        .unwrap();
    let list = get_payment_list(&setup.public_storage, &setup.public_key, &receiver_path())
        .await
        .unwrap();
    assert_eq!(
        list.payment_endpoints,
        vec![(lightning.clone(), lightning_data.clone())]
            .into_iter()
            .collect()
    );

    remove_payment_endpoint(&setup.session, &receiver_path(), lightning.clone())
        .await
        .unwrap();
    let empty = get_payment_list(&setup.public_storage, &setup.public_key, &receiver_path())
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
            &receiver_path(),
            identifier.clone(),
            payload.clone(),
        )
        .await
        .unwrap();
        expected.insert(identifier, payload);
    }

    let list = get_payment_list(&setup.public_storage, &setup.public_key, &receiver_path())
        .await
        .unwrap();
    assert_eq!(list.payment_endpoints, expected);

    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn removing_missing_endpoint_is_idempotent() {
    let setup = TestSetup::new().await;
    let method = PaymentEndpointIdentifier::new("unused").unwrap();

    remove_payment_endpoint(&setup.session, &receiver_path(), method)
        .await
        .expect("removing non-existent endpoint should be idempotent");

    setup.raw_session.signout().await.unwrap();
}
