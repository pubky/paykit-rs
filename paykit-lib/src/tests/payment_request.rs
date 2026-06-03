use serde_json::{json, Map as JsonMap, Value as JsonValue};

use super::*;

fn object(value: JsonValue) -> JsonMap<String, JsonValue> {
    match value {
        JsonValue::Object(map) => map,
        _ => panic!("test value must be an object"),
    }
}

fn request_id() -> PaymentRequestId {
    PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap()
}

fn payment_request_terms() -> PaymentRequestTerms {
    PaymentRequestTerms {
        amount: PaymentAmount {
            value: "0.001".to_string(),
            asset: "btc".to_string(),
        },
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        proposal_expires_at: Some("2026-06-01T00:00:00Z".to_string()),
        recurrence: None,
        accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
            "btc-lightning-bolt11",
        )
        .unwrap()],
        metadata: object(json!({"label": "test invoice"})),
    }
}

fn payment_request() -> PaymentRequest {
    PaymentRequest::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101").unwrap(),
        request_id(),
        payment_request_terms(),
    )
}

fn recurring_payment_request() -> PaymentRequest {
    let mut terms = payment_request_terms();
    terms.payment_reference = PaymentReference::new("subscription-2026-0001").unwrap();
    terms.recurrence = Some(Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        anchor: "2026-06-01T00:00:00Z".to_string(),
        ends_at: Some("2026-12-01T00:00:00Z".to_string()),
    });
    PaymentRequest::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d106").unwrap(),
        request_id(),
        terms,
    )
}

fn payment_proof() -> PaymentProof {
    PaymentProof::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105").unwrap(),
        request_id(),
        PaymentReference::new("invoice-2026-0001").unwrap(),
        None,
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        object(json!({
            "type": "bitcoin-bolt11-preimage",
            "data": "preimage"
        })),
    )
}

fn recurring_payment_proof() -> PaymentProof {
    PaymentProof::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d107").unwrap(),
        request_id(),
        PaymentReference::new("subscription-2026-0001").unwrap(),
        Some(BillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".to_string(),
            ends_at: "2026-07-01T00:00:00Z".to_string(),
        }),
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        object(json!({
            "type": "bitcoin-bolt11-preimage",
            "data": "recurring-preimage"
        })),
    )
}

fn private_application_message(
    kind: PrivateMessageKind,
    raw_json: &str,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some(kind.as_str().to_string()),
        raw_json: raw_json.to_string(),
    }
}

fn parse_payment_request_raw(
    kind: PrivateMessageKind,
    raw_json: &str,
) -> PaymentRequestEventMessage {
    parse_payment_request_event_message(&private_application_message(kind, raw_json))
        .expect("message kind should be a Payment Request event")
}

#[tokio::test]
async fn payment_request_events_are_global_fifo() {
    let mut setup = PrivateTestSetup::new().await;
    let request = payment_request();
    let acceptance = PaymentRequestAcceptance::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
        request_id(),
    );
    let rejection = PaymentRequestRejection::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103").unwrap(),
        request_id(),
        Some("declined_by_payer".to_string()),
    );
    let proof = payment_proof();
    let cancellation = PaymentRequestCancellation::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d108").unwrap(),
        request_id(),
        Some("user_requested".to_string()),
    );

    send_payment_request(&mut setup.sender_link, &request)
        .await
        .unwrap();
    send_payment_request_acceptance(&mut setup.sender_link, &acceptance)
        .await
        .unwrap();
    send_payment_request_rejection(&mut setup.sender_link, &rejection)
        .await
        .unwrap();
    send_payment_proof(&mut setup.sender_link, &proof)
        .await
        .unwrap();
    send_payment_request_cancellation(&mut setup.sender_link, &cancellation)
        .await
        .unwrap();

    let events = receive_payment_request_events_for_test(&mut setup.receiver_link).await;

    assert_eq!(events.len(), 5);
    assert_eq!(
        events[0].parsed_event(),
        Some(&PaymentRequestEvent::Request(request.clone()))
    );
    assert_eq!(
        events[1].parsed_event(),
        Some(&PaymentRequestEvent::Acceptance(acceptance))
    );
    assert_eq!(
        events[2].parsed_event(),
        Some(&PaymentRequestEvent::Rejection(rejection))
    );
    assert_eq!(
        events[3].parsed_event(),
        Some(&PaymentRequestEvent::Proof(proof.clone()))
    );
    assert_eq!(
        events[4].parsed_event(),
        Some(&PaymentRequestEvent::Cancellation(cancellation))
    );
    assert!(events[0]
        .raw_json
        .contains("\"kind\":\"paykit.payment_request\""));
    assert!(events[1]
        .raw_json
        .contains("\"kind\":\"paykit.payment_request_acceptance\""));
    assert!(events[2]
        .raw_json
        .contains("\"kind\":\"paykit.payment_request_rejection\""));
    assert!(events[3]
        .raw_json
        .contains("\"kind\":\"paykit.payment_proof\""));
    assert!(events[4]
        .raw_json
        .contains("\"kind\":\"paykit.payment_request_cancellation\""));
    assert_eq!(events[0].event_id(), Some(&request.event_id));
    assert_eq!(
        events[3].payment_request_id(),
        Some(&proof.payment_request_id)
    );
    assert_eq!(proof.payment_reference.as_str(), "invoice-2026-0001");

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[test]
fn payment_request_events_return_exact_raw_payload() {
    let raw = r#"{
        "kind": "paykit.payment_request",
        "version": 1,
        "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "request": {
            "metadata": { "note": "preserve raw formatting" },
            "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
            "recurrence": null,
            "proposal_expires_at": null,
            "payment_reference": "invoice-2026-raw",
            "amount": { "asset": "btc", "value": "0.001" }
        }
    }"#;

    let event = parse_payment_request_raw(PrivateMessageKind::PaymentRequest, raw);

    assert_eq!(event.raw_json, raw);
    assert_eq!(
        event.event_id().map(EventId::as_str),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
    );
}

#[test]
fn payment_request_event_serialization_supports_outbound_idempotency() {
    let request = payment_request();
    let event = PaymentRequestEvent::Request(request.clone());

    let serialized = serialize_payment_request_event(&event).unwrap();
    let parsed = parse_payment_request_raw(PrivateMessageKind::PaymentRequest, &serialized);

    assert_eq!(parsed.parsed_event(), Some(&event));
    assert_eq!(parsed.event_id(), Some(&request.event_id));
    assert!(serialized.contains("\"proposal_expires_at\""));
}

#[test]
fn payment_request_event_parser_uses_raw_json_kind() {
    let request = payment_request();
    let serialized =
        serialize_payment_request_event(&PaymentRequestEvent::Request(request.clone())).unwrap();
    let stale_message = private_application_message(PrivateMessageKind::ReceiptAccess, &serialized);

    let parsed = parse_payment_request_event_message(&stale_message)
        .expect("raw JSON kind should route to Payment Request parser");

    assert_eq!(parsed.kind(), PrivateMessageKind::PaymentRequest);
    assert_eq!(
        parsed.parsed_event(),
        Some(&PaymentRequestEvent::Request(request))
    );
}

#[test]
fn payment_request_events_return_malformed_recognized_events_for_persistence() {
    let malformed = r#"{"version":1,"kind":"paykit.payment_request","event_id":"not-a-uuid","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"0.001","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;
    let acceptance = PaymentRequestAcceptance::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
        request_id(),
    );

    let malformed_event = parse_payment_request_raw(PrivateMessageKind::PaymentRequest, malformed);
    let acceptance_raw = json!({
        "version": 1,
        "kind": "paykit.payment_request_acceptance",
        "event_id": acceptance.event_id.as_str(),
        "payment_request_id": acceptance.payment_request_id.as_str(),
    })
    .to_string();
    let acceptance_event = parse_payment_request_raw(
        PrivateMessageKind::PaymentRequestAcceptance,
        &acceptance_raw,
    );

    assert_eq!(malformed_event.kind(), PrivateMessageKind::PaymentRequest);
    assert_eq!(malformed_event.raw_json, malformed);
    assert!(!malformed_event.is_valid());
    assert!(malformed_event.parsed_event().is_none());
    assert!(malformed_event
        .validation_error()
        .is_some_and(|err| err.contains("Event ID")));
    assert_eq!(malformed_event.event_id(), None);
    assert_eq!(malformed_event.payment_request_id(), Some(&request_id()));
    assert_eq!(
        acceptance_event.parsed_event(),
        Some(&PaymentRequestEvent::Acceptance(acceptance))
    );
}

#[test]
fn payment_request_events_keep_valid_ids_when_body_is_invalid() {
    let malformed = r#"{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"ten","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;

    let event = parse_payment_request_raw(PrivateMessageKind::PaymentRequest, malformed);

    assert_eq!(event.kind(), PrivateMessageKind::PaymentRequest);
    assert_eq!(
        event.event_id().map(EventId::as_str),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
    );
    assert_eq!(
        event.payment_request_id().map(PaymentRequestId::as_str),
        Some("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33")
    );
    assert!(!event.is_valid());
    assert!(event
        .validation_error()
        .is_some_and(|err| err.contains("amount.value")));
    assert_eq!(event.raw_json, malformed);
}

#[test]
fn payment_request_events_surface_conflicts_for_caller_dedupe() {
    let first = r#"{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"0.001","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;
    let conflicting = r#"{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"0.002","asset":"btc"},"payment_reference":"invoice-2026-0002","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;

    let first_event = parse_payment_request_raw(PrivateMessageKind::PaymentRequest, first);
    let conflicting_event =
        parse_payment_request_raw(PrivateMessageKind::PaymentRequest, conflicting);

    assert_eq!(first_event.raw_json, first);
    assert_eq!(conflicting_event.raw_json, conflicting);
    assert_eq!(first_event.event_id(), conflicting_event.event_id());
    assert_ne!(first_event.raw_json, conflicting_event.raw_json);
}

#[tokio::test]
async fn payment_request_events_share_ordered_stream_with_other_private_lanes() {
    let mut setup = PrivateTestSetup::new().await;
    let receipt_access_json =
        r#"{"version":1,"kind":"paykit.receipt_access","payload":"raw-only"}"#;
    let request = payment_request();
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(endpoint.clone(), PaymentEndpointPayload::new("lnbc1..."));
    let private_list = private_payment_list(&payment_endpoints);

    send_raw_private_message(&mut setup.sender_link, receipt_access_json).await;
    send_payment_request(&mut setup.sender_link, &request)
        .await
        .unwrap();
    set_private_payment_list(&mut setup.sender_link, &private_list)
        .await
        .unwrap();

    let messages = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.kind.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some(PrivateMessageKind::ReceiptAccess.as_str()),
            Some(PrivateMessageKind::PaymentRequest.as_str()),
            Some(PrivateMessageKind::PrivatePaymentList.as_str()),
        ]
    );
    assert_eq!(messages[0].raw_json, receipt_access_json);
    assert_eq!(
        messages[1].known_kind(),
        Some(PrivateMessageKind::PaymentRequest)
    );
    assert_eq!(
        messages[2].known_kind(),
        Some(PrivateMessageKind::PrivatePaymentList)
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn recurring_payment_request_and_proof_with_billing_period_round_trip() {
    let mut setup = PrivateTestSetup::new().await;
    let request = recurring_payment_request();
    let proof = recurring_payment_proof();

    send_payment_request(&mut setup.sender_link, &request)
        .await
        .unwrap();
    send_payment_proof(&mut setup.sender_link, &proof)
        .await
        .unwrap();

    let events = receive_payment_request_events_for_test(&mut setup.receiver_link).await;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].parsed_event(),
        Some(&PaymentRequestEvent::Request(request))
    );
    assert_eq!(
        events[1].parsed_event(),
        Some(&PaymentRequestEvent::Proof(proof.clone()))
    );
    assert_eq!(
        proof
            .billing_period
            .as_ref()
            .map(|period| (period.starts_at.as_str(), period.ends_at.as_str())),
        Some(("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z"))
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}
