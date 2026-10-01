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
async fn test_endpoint_repair_and_removal_use_raw_revisions() {
    let setup = TestSetup::new().await;
    let app = app_id();
    let identifier = PaymentEndpointIdentifier::new("btc-onchain").unwrap();
    let path = pubky_routing::payment_endpoint_path(&app, &identifier);
    for bytes in [
        vec![0xff],
        vec![b'x'; PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES + 1],
    ] {
        setup
            .session
            .storage()
            .put(&path, bytes.clone())
            .await
            .unwrap();
        assert!(
            get_payment_endpoint(&setup.public_storage, &setup.public_key, &app, &identifier)
                .await
                .is_err()
        );
        let revision =
            pubky_routing::fetch_payment_endpoint_revision(&setup.session, &app, &identifier)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(revision, content_revision(&bytes));

        update_payment_endpoint(
            &setup.session,
            &app,
            identifier.clone(),
            PaymentEndpointPayload::new("repaired"),
            &revision,
        )
        .await
        .unwrap();
        let error = remove_payment_endpoint_if_revision(
            &setup.session,
            &app,
            identifier.clone(),
            &revision,
        )
        .await
        .unwrap_err();
        assert!(is_write_conflict(&error));

        setup.session.storage().put(&path, bytes).await.unwrap();
        remove_payment_endpoint_if_revision(&setup.session, &app, identifier.clone(), &revision)
            .await
            .unwrap();
        assert!(
            pubky_routing::fetch_payment_endpoint_revision(&setup.session, &app, &identifier)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn test_malformed_registry_can_be_conditionally_repaired() {
    let setup = TestSetup::new().await;
    setup
        .session
        .storage()
        .put(PAYKIT_APP_REGISTRY_PATH, vec![0xff])
        .await
        .unwrap();
    assert!(
        get_paykit_app_registry_with_revision(&setup.public_storage, &setup.public_key)
            .await
            .is_err()
    );
    let resource = format!("{}{PAYKIT_APP_REGISTRY_PATH}", setup.public_key)
        .parse()
        .unwrap();
    let revision = pubky_routing::fetch_resource_revision(
        &setup.public_storage,
        &resource,
        PAYKIT_APP_REGISTRY_MAX_BYTES,
    )
    .await
    .unwrap()
    .unwrap();
    let registry = PaykitAppRegistry::new(None);
    update_paykit_app_registry(&setup.session, &registry, &revision)
        .await
        .unwrap();
    assert_eq!(
        get_paykit_app_registry(&setup.public_storage, &setup.public_key)
            .await
            .unwrap(),
        Some(registry.clone())
    );
    assert!(is_write_conflict(
        &update_paykit_app_registry(&setup.session, &registry, &revision)
            .await
            .unwrap_err()
    ));
}

#[tokio::test]
async fn test_endpoint_cleanup_listing_accepts_overfull_lists() {
    let setup = TestSetup::new().await;
    let app = app_id();
    for index in 0..=PAYMENT_LIST_MAX_ENDPOINTS {
        let identifier = PaymentEndpointIdentifier::new(format!("endpoint-{index:03}")).unwrap();
        setup
            .session
            .storage()
            .put(
                pubky_routing::payment_endpoint_path(&app, &identifier),
                vec![0xff],
            )
            .await
            .unwrap();
    }
    assert!(
        get_payment_list(&setup.public_storage, &setup.public_key, &app)
            .await
            .is_err()
    );
    let identifiers = pubky_routing::list_payment_endpoint_identifiers(
        &setup.public_storage,
        &setup.public_key,
        &app,
    )
    .await
    .unwrap();
    assert_eq!(identifiers.len(), PAYMENT_LIST_MAX_ENDPOINTS + 1);
}

#[tokio::test]
async fn test_payment_list_exact_byte_budget_allows_trailing_empty_endpoint() {
    let setup = TestSetup::new().await;
    for (name, body) in [("a", "full"), ("b", "")] {
        set_payment_endpoint(
            &setup.session,
            &app_id(),
            PaymentEndpointIdentifier::new(name).unwrap(),
            PaymentEndpointPayload::new(body),
        )
        .await
        .unwrap();
    }
    let list =
        get_payment_list_with_limits(&setup.public_storage, &setup.public_key, &app_id(), 2, 4)
            .await
            .unwrap();
    assert_eq!(list.payment_endpoints.len(), 1);
}

#[tokio::test]
async fn test_write_lock_is_renewed_and_released_after_failure() {
    let setup = TestSetup::new().await;
    let path = format!("{PAYKIT_PATH_PREFIX}lock-test.json");
    let storage = setup.session.storage();
    let error = pubky_routing::with_write_lock_timeout(
        &setup.session,
        &path,
        std::time::Duration::from_secs(6),
        |lock| async move {
            tokio::time::sleep(lock.timeout() + std::time::Duration::from_secs(1)).await;
            let conflict = storage.lock(lock.path(), lock.timeout()).await.unwrap_err();
            assert!(matches!(conflict,
                pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
                    if status == pubky::StatusCode::LOCKED
            ));
            storage.put_locked(&lock, "renewed").await.unwrap();
            Err::<(), _>(PaykitError::Validation("operation failed".into()))
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PaykitError::Validation(_)));
    let lock = setup
        .session
        .storage()
        .lock(&path, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let stored = setup
        .session
        .storage()
        .get(&path)
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(stored, "renewed");
    setup.session.storage().unlock(&lock).await.unwrap();
    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_write_lock_waits_for_another_writer_before_running_operation() {
    let setup = TestSetup::new().await;
    let path = format!("{PAYKIT_PATH_PREFIX}contended.json");
    let storage = setup.session.storage();
    let lock = storage
        .lock(&path, std::time::Duration::from_secs(60))
        .await
        .unwrap();
    let waiting_storage = &storage;
    let operation = with_write_lock(&setup.session, &path, |lock| async move {
        waiting_storage
            .put_locked(&lock, "next writer")
            .await
            .unwrap();
        Ok::<_, PaykitError>(())
    });
    tokio::pin!(operation);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut operation)
            .await
            .is_err()
    );
    storage.unlock(&lock).await.unwrap();
    operation.await.unwrap();
    assert_eq!(
        storage.get(&path).await.unwrap().text().await.unwrap(),
        "next writer"
    );
}

#[tokio::test]
async fn test_empty_registry_is_invalid_for_both_read_apis() {
    let setup = TestSetup::new().await;
    setup
        .session
        .storage()
        .put(PAYKIT_APP_REGISTRY_PATH, Vec::new())
        .await
        .unwrap();
    assert!(matches!(
        get_paykit_app_registry(&setup.public_storage, &setup.public_key).await,
        Err(PaykitError::InvalidData { .. })
    ));
    assert!(matches!(
        get_paykit_app_registry_with_revision(&setup.public_storage, &setup.public_key).await,
        Err(PaykitError::InvalidData { .. })
    ));
}

#[tokio::test]
async fn test_renewal_loss_after_commit_is_not_a_retryable_conflict() {
    let setup = TestSetup::new().await;
    let path = format!("{PAYKIT_PATH_PREFIX}uncertain-write.json");
    let storage = setup.session.storage();
    let error = pubky_routing::with_write_lock_timeout(
        &setup.session,
        &path,
        std::time::Duration::from_secs(6),
        |lock| async move {
            storage.put_locked(&lock, "committed").await.unwrap();
            // Model ownership lost while the committed write's response is unresolved.
            storage.unlock(&lock).await.unwrap();
            std::future::pending::<Result<()>>().await
        },
    )
    .await
    .unwrap_err();
    assert!(!is_write_conflict(&error));
    assert!(matches!(error, PaykitError::Transport { .. }));
    let stored = setup
        .session
        .storage()
        .get(&path)
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(stored, "committed");
    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_cancelled_write_does_not_unlock_an_unfinished_operation() {
    let setup = TestSetup::new().await;
    let path = format!("{PAYKIT_PATH_PREFIX}cancelled-write.json");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let lock = {
        let operation = with_write_lock(&setup.session, &path, |lock| async {
            started_tx.send(lock).unwrap();
            std::future::pending::<Result<()>>().await
        });
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("operation must remain pending: {result:?}"),
            lock = started_rx => lock.unwrap(),
        }
    };
    let conflict = setup
        .session
        .storage()
        .lock(&path, lock.timeout())
        .await
        .unwrap_err();
    assert!(matches!(conflict,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
            if status == pubky::StatusCode::LOCKED
    ));
    setup.session.storage().unlock(&lock).await.unwrap();
    setup.raw_session.signout().await.unwrap();
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
    let versioned = get_paykit_app_registry_with_revision(&setup.public_storage, &setup.public_key)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(fetched, Some(registry));
    assert_eq!(versioned.0, fetched.unwrap());
    assert!(!versioned.1.is_empty());
    setup.raw_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_app_registry_rejects_stale_revision_update() {
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

    let (mut first_update, revision) =
        get_paykit_app_registry_with_revision(&setup.public_storage, &setup.public_key)
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

    update_paykit_app_registry(&setup.session, &first_update, &revision)
        .await
        .unwrap();
    let error = update_paykit_app_registry(&setup.session, &stale_update, &revision)
        .await
        .unwrap_err();
    assert!(pubky_routing::is_write_conflict(&error));

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
    assert!(pubky_routing::is_write_conflict(&stale_delete));

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
