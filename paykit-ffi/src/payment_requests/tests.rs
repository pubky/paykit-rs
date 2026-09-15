use std::sync::Arc;

use chrono::Utc;
use paykit_lib::{PaymentRequestTerms, RecurrenceUnit};
use paykit_sdk::{
    AmountRecord, BillingPeriodRecord, OutboundPrivateMessageStatus, PaymentProofRecord,
    PaymentRequestFilter, PaymentRequestLifecycleState, PaymentRequestLocalRole,
    PaymentRequestRecord, PaymentRequestTermsRecord, PubkyPublicKey,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::*;

const ALLOWANCE_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";

fn public_key() -> PubkyPublicKey {
    conversions::parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into())
        .unwrap()
}

#[test]
fn test_payment_request_terms_parse_protocol_inputs() {
    let terms = FfiPaymentRequestTerms {
        amount: FfiPaymentRequestAmount {
            value: "25.50".into(),
            asset: "usd".into(),
        },
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        proposal_expires_at: Some("2026-06-18T12:00:00Z".into()),
        recurrence: Some(FfiPaymentRequestRecurrence {
            every: 1,
            unit: "month".into(),
            starts_at: "2026-06-01T00:00:00Z".into(),
            anchor: "2026-06-01T00:00:00Z".into(),
            ends_at: None,
        }),
        accepted_payment_endpoint_identifiers: vec!["btc-lightning-bolt11".into()],
        metadata: Arc::new(FfiPrivateJsonObject::new(r#"{"order":"123"}"#.into()).unwrap()),
    };

    let parsed = PaymentRequestTerms::try_from(terms).unwrap();

    assert_eq!(parsed.amount().value(), "25.50");
    assert_eq!(parsed.payment_reference().as_str(), "invoice-1");
    assert!(matches!(
        parsed
            .recurrence()
            .as_ref()
            .map(|recurrence| recurrence.unit()),
        Some(RecurrenceUnit::Month)
    ));
    assert_eq!(
        parsed
            .metadata()
            .get("order")
            .and_then(serde_json::Value::as_str),
        Some("123")
    );
}

#[test]
fn test_payment_reference_debug_redacts_text() {
    let reference = FfiPaymentReference::new("invoice secret".into()).unwrap();

    assert_eq!(reference.export_text(), "invoice secret");
    assert!(!format!("{reference:?}").contains("invoice secret"));
}

#[test]
fn test_payment_request_filter_rejects_unknown_state() {
    let filter = FfiPaymentRequestFilter {
        counterparty: None,
        counterparty_receiver_path: None,
        local_role: None,
        states: vec![FfiPaymentRequestLifecycleState::Unknown],
        recurring: None,
        received_only: false,
    };

    assert!(matches!(
        PaymentRequestFilter::try_from(filter),
        Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
    ));
}

#[test]
fn test_payment_request_record_conversion_redacts_references() {
    let mut metadata = JsonMap::new();
    metadata.insert("source".into(), JsonValue::String("test".into()));
    let mut proof = JsonMap::new();
    proof.insert("preimage".into(), JsonValue::String("secret".into()));

    let record = PaymentRequestRecord {
        counterparty: public_key(),
        counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        payment_request_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        local_role: Some(PaymentRequestLocalRole::Payer),
        state: PaymentRequestLifecycleState::Accepted,
        proposal_stream_item_id: Some(1),
        proposal_outbound_message_id: None,
        proposal_outbound_status: None,
        proposal_event_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
        terms: Some(PaymentRequestTermsRecord {
            amount: AmountRecord {
                value: "10".into(),
                asset: "usd".into(),
            },
            payment_reference: "invoice secret".into(),
            proposal_expires_at: None,
            recurrence: None,
            accepted_payment_endpoint_identifiers: vec!["btc-lightning-bolt11".into()],
            metadata,
        }),
        accepted_event_id: None,
        accepted_outbound_status: Some(OutboundPrivateMessageStatus::Pending),
        rejected_event_id: None,
        rejected_outbound_status: None,
        canceled_event_id: None,
        canceled_outbound_status: None,
        payment_proofs: vec![PaymentProofRecord {
            event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
            outbound_message_id: Some(9),
            outbound_status: Some(OutboundPrivateMessageStatus::Sent),
            stream_item_id: None,
            payment_reference: "invoice secret".into(),
            billing_period: Some(BillingPeriodRecord {
                starts_at: "2026-06-01T00:00:00Z".into(),
                ends_at: "2026-07-01T00:00:00Z".into(),
            }),
            payment_endpoint_identifier: "btc-lightning-bolt11".into(),
            allowance_id: Some(ALLOWANCE_ID.into()),
            proof,
            recorded_at: Utc::now(),
        }],
        last_stream_item_id: Some(1),
        last_outbound_message_id: Some(9),
        last_outbound_status: Some(OutboundPrivateMessageStatus::Sent),
        last_event_at: Some(Utc::now()),
        invalid_reason: None,
    };

    let ffi = FfiPaymentRequestRecord::try_from(record).unwrap();

    assert_eq!(ffi.state, FfiPaymentRequestLifecycleState::Accepted);
    assert_eq!(
        ffi.payment_proofs[0].allowance_id.as_deref(),
        Some(ALLOWANCE_ID)
    );
    assert_eq!(
        ffi.terms.as_ref().unwrap().payment_reference.export_text(),
        "invoice secret"
    );
    assert!(!format!("{:?}", ffi.terms.unwrap().payment_reference).contains("invoice secret"));
    assert!(ffi.payment_proofs[0]
        .proof
        .export_text()
        .contains("\"preimage\":\"secret\""));
    assert!(!format!("{:?}", ffi.payment_proofs[0].proof).contains("secret"));
}

#[test]
fn test_payment_proof_submission_rejects_non_object_proof() {
    let submission = FfiPaymentProofSubmission {
        billing_period: None,
        payment_endpoint_identifier: "btc-lightning-bolt11".into(),
        allowance_id: None,
        proof: Arc::new(FfiPrivateJsonObject::from_unchecked_text("[]".into())),
    };

    assert!(matches!(
        PaymentProofSubmission::try_from(submission),
        Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
    ));
}

fn proof_submission(allowance_id: Option<String>) -> FfiPaymentProofSubmission {
    FfiPaymentProofSubmission {
        billing_period: None,
        payment_endpoint_identifier: "btc-lightning-bolt11".into(),
        allowance_id,
        proof: Arc::new(FfiPrivateJsonObject::new("{}".into()).unwrap()),
    }
}

#[test]
fn test_payment_proof_submission_preserves_optional_allowance_id() {
    for id in [None, Some(ALLOWANCE_ID.to_owned())] {
        let parsed = PaymentProofSubmission::try_from(proof_submission(id.clone())).unwrap();
        assert_eq!(
            parsed.allowance_id.as_ref().map(|id| id.as_str()),
            id.as_deref()
        );
    }
}

#[test]
fn test_payment_proof_submission_rejects_invalid_allowance_id_without_leaking_input() {
    for id in [
        "secret invalid allowance value".to_owned(),
        ALLOWANCE_ID.to_uppercase(),
        ALLOWANCE_ID.replace('-', ""),
        ALLOWANCE_ID.replacen("4b0e", "1b0e", 1),
        String::new(),
    ] {
        let submission = proof_submission(Some(id.clone()));
        if !id.is_empty() {
            assert!(!format!("{submission:?}").contains(&id));
        }
        let Err(error) = PaymentProofSubmission::try_from(submission) else {
            panic!("invalid Allowance ID must be rejected");
        };
        assert!(matches!(&error, PaykitFfiError::Protocol { code, .. } if code == "validation"));
        if !id.is_empty() {
            assert!(!format!("{error:?}").contains(&id));
        }
    }
}

#[test]
fn test_payment_proof_record_preserves_absent_allowance_id() {
    let record = PaymentProofRecord {
        event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
        outbound_message_id: None,
        outbound_status: None,
        stream_item_id: Some(1),
        payment_reference: "invoice-1".into(),
        billing_period: None,
        payment_endpoint_identifier: "btc-lightning-bolt11".into(),
        allowance_id: None,
        proof: JsonMap::new(),
        recorded_at: Utc::now(),
    };
    assert!(FfiPaymentProofRecord::try_from(record)
        .unwrap()
        .allowance_id
        .is_none());
}

#[test]
fn test_billing_period_conversion_rejects_invalid_interval() {
    for ends_at in ["invalid", "2026-06-01T00:00:00Z", "2026-05-01T00:00:00Z"] {
        let result = paykit_lib::BillingPeriod::try_from(FfiBillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".into(),
            ends_at: ends_at.into(),
        });
        assert!(
            matches!(result, Err(PaykitFfiError::Protocol { code, .. }) if code == "validation")
        );
    }
}

#[test]
fn test_recurrence_conversion_rejects_zero_interval() {
    let result = paykit_lib::Recurrence::try_from(FfiPaymentRequestRecurrence {
        every: 0,
        unit: "month".into(),
        starts_at: "2026-06-01T00:00:00Z".into(),
        anchor: "2026-06-01T00:00:00Z".into(),
        ends_at: None,
    });
    assert!(matches!(result, Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"));
}

#[test]
fn test_terms_conversion_rejects_empty_endpoint_list() {
    let result = PaymentRequestTerms::try_from(FfiPaymentRequestTerms {
        amount: FfiPaymentRequestAmount {
            value: "1".into(),
            asset: "btc".into(),
        },
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        proposal_expires_at: None,
        recurrence: None,
        accepted_payment_endpoint_identifiers: Vec::new(),
        metadata: Arc::new(FfiPrivateJsonObject::new("{}".into()).unwrap()),
    });
    assert!(matches!(result, Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"));
}
