use super::*;

fn bound_request(payload: &str) -> PaymentRequest {
    let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let terms = paykit_lib::PaymentRequestTerms::builder(
        paykit_lib::PaymentAmount::new("0.001", "btc").unwrap(),
        paykit_lib::PaymentReference::new("invoice-1").unwrap(),
        vec![identifier.clone()],
    )
    .required_app_id(Some(app_id()))
    .payment_endpoints(Some(HashMap::from([(
        identifier,
        paykit_lib::PaymentEndpointPayload::new(payload),
    )])))
    .build()
    .unwrap();
    PaymentRequest::new(
        EventId::new_v4(),
        paykit_lib::PaymentRequestId::new_v4(),
        terms,
    )
}

#[test]
fn test_payment_endpoints_record_postcard_round_trip() {
    let request = bound_request("private-invoice");
    let mut terms = PaymentRequestTermsRecord::from(request.request());
    for endpoints in [terms.payment_endpoints.clone(), None] {
        terms.payment_endpoints = endpoints;
        let encoded = postcard::to_allocvec(&terms).unwrap();
        let decoded: PaymentRequestTermsRecord = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, terms);
        assert!(!format!("{decoded:?}").contains("private-invoice"));
    }
}

#[test]
fn test_payment_endpoints_record_reconstruction_validates_bindings() {
    let request = bound_request("private-invoice");
    let mut record =
        PaymentRequestRecord::new(counterparty(), request.payment_request_id().to_string());
    record.proposal_event_id = Some(request.event_id().to_string());
    record.terms = Some(PaymentRequestTermsRecord::from(request.request()));
    assert_eq!(request_from_record(&record).unwrap(), request);

    for endpoints in [
        HashMap::new(),
        HashMap::from([("btc-lightning-bolt11".into(), "".into())]),
        HashMap::from([("private".into(), "private-invoice".into())]),
        HashMap::from([("../btc".into(), "private-invoice".into())]),
        HashMap::from([("eur-sepa-iban".into(), "private-iban".into())]),
    ] {
        record.terms.as_mut().unwrap().payment_endpoints = Some(endpoints);
        assert!(request_from_record(&record).is_none());
    }
    record.terms = Some(PaymentRequestTermsRecord::from(request.request()));
    record.terms.as_mut().unwrap().required_app_id = None;
    assert!(request_from_record(&record).is_none());
    record.terms.as_mut().unwrap().payment_endpoints = None;
    assert!(request_from_record(&record)
        .unwrap()
        .request()
        .payment_endpoints()
        .is_none());
}

#[tokio::test]
async fn test_payment_endpoints_survive_reducer_storage_and_backup() {
    let storage = registered_storage();
    let peer = counterparty();
    let request = bound_request("private-invoice");
    let record = enqueue_payment_request(&storage, peer.clone(), &app_id(), &request, timestamp())
        .await
        .unwrap();
    assert_eq!(request_from_record(&record).unwrap(), request);

    let bytes = crate::storage::encode_storage_state_blob(&storage.snapshot().unwrap()).unwrap();
    let storage =
        InMemoryStorage::from_state(crate::storage::decode_storage_state_blob(&bytes).unwrap());
    let backup = crate::backup::export_backup_state(&storage).await.unwrap();
    let restored = InMemoryStorage::new();
    crate::backup::restore_backup_state(&restored, backup)
        .await
        .unwrap();
    let records = payment_request_records(&restored, &peer, timestamp())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(request_from_record(&records[0]).unwrap(), request);

    let inbound = InMemoryStorage::new();
    let raw =
        serialize_payment_request_event(&app_id(), &PaymentRequestEvent::Request(request.clone()))
            .unwrap();
    persist_messages(&inbound, peer.clone(), vec![raw]).await;
    let records = received_payment_request_records(&inbound, &peer, timestamp())
        .await
        .unwrap();
    assert_eq!(request_from_record(&records[0]).unwrap(), request);
}

#[tokio::test]
async fn test_payment_endpoints_reducer_keeps_original_bindings_on_conflict() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let request = bound_request("original-private-invoice");
    let replacement = PaymentRequest::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        bound_request("replacement-private-invoice")
            .request()
            .clone(),
    );
    let messages = [request.clone(), replacement]
        .into_iter()
        .map(|request| {
            serialize_payment_request_event(&app_id(), &PaymentRequestEvent::Request(request))
                .unwrap()
        })
        .collect();
    persist_messages(&storage, peer.clone(), messages).await;
    let records = received_payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert_eq!(request_from_record(&records[0]).unwrap(), request);
}

#[tokio::test]
async fn test_payment_endpoints_enforce_existing_message_size_limit() {
    let limit = paykit_lib::pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN;
    let base = bound_request("x");
    let raw =
        serialize_payment_request_event(&app_id(), &PaymentRequestEvent::Request(base)).unwrap();
    let padding = limit - raw.len();
    for extra in [0, 1] {
        let storage = registered_storage();
        let request = bound_request(&"x".repeat(1 + padding + extra));
        let raw = serialize_payment_request_event(
            &app_id(),
            &PaymentRequestEvent::Request(request.clone()),
        )
        .unwrap();
        assert_eq!(raw.len(), limit + extra);
        let before = storage.snapshot().unwrap();
        let result =
            enqueue_payment_request(&storage, counterparty(), &app_id(), &request, timestamp())
                .await;
        if extra == 0 {
            assert!(result.is_ok());
        } else {
            assert!(
                matches!(result, Err(PaykitSdkError::Protocol { context, .. }) if context.contains("message size"))
            );
            assert_eq!(storage.snapshot().unwrap(), before);
        }
    }
}
