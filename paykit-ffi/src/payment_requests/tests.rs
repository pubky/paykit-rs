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

fn bound_terms() -> FfiPaymentRequestTerms {
    FfiPaymentRequestTerms {
        amount: FfiPaymentRequestAmount {
            value: "0.001".into(),
            asset: "btc".into(),
        },
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        proposal_expires_at: None,
        recurrence: None,
        accepted_payment_endpoint_identifiers: vec!["btc-lightning-bolt11".into()],
        required_app_id: Some("bitkit".into()),
        payment_endpoints: Some(HashMap::from([(
            "btc-lightning-bolt11".into(),
            "private-invoice".into(),
        )])),
        conversion: None,
        payment_deadline: None,
        metadata: Arc::new(FfiPrivateJsonObject::new("{}".into()).unwrap()),
    }
}

#[test]
fn test_payment_endpoints_ffi_round_trip_and_redaction() {
    for endpoints in [bound_terms().payment_endpoints, None] {
        let mut ffi = bound_terms();
        ffi.payment_endpoints = endpoints;
        let native = PaymentRequestTerms::try_from(ffi.clone()).unwrap();
        let record = PaymentRequestTermsRecord::from(&native);
        let restored = FfiPaymentRequestTerms::try_from(record).unwrap();
        assert_eq!(restored.payment_endpoints, ffi.payment_endpoints);
        assert!(!format!("{restored:?}").contains("private-invoice"));
        assert_eq!(PaymentRequestTerms::try_from(restored).unwrap(), native);
    }
}

#[test]
fn test_payment_endpoints_ffi_rejects_invalid_input() {
    for endpoints in [
        HashMap::new(),
        HashMap::from([("btc-lightning-bolt11".into(), "".into())]),
        HashMap::from([("private".into(), "private-invoice".into())]),
        HashMap::from([("../btc".into(), "private-invoice".into())]),
        HashMap::from([("eur-sepa-iban".into(), "private-iban".into())]),
    ] {
        let mut ffi = bound_terms();
        ffi.payment_endpoints = Some(endpoints);
        assert!(
            matches!(PaymentRequestTerms::try_from(ffi), Err(PaykitFfiError::Protocol { code, .. }) if code == "validation")
        );
    }
    let mut ffi = bound_terms();
    ffi.required_app_id = None;
    assert!(PaymentRequestTerms::try_from(ffi).is_err());
}

fn public_key() -> PubkyPublicKey {
    conversions::parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into())
        .unwrap()
}

#[test]
fn test_payment_request_terms_parse_protocol_inputs() {
    let terms = FfiPaymentRequestTerms {
        payment_endpoints: None,
        conversion: None,
        payment_deadline: None,
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
        required_app_id: Some("bitkit".into()),
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
        conversion_quotes: Vec::new(),
        counterparty: public_key(),
        payment_request_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        local_role: Some(PaymentRequestLocalRole::Payer),
        state: PaymentRequestLifecycleState::Accepted,
        proposal_stream_item_id: Some(1),
        proposal_outbound_message_id: None,
        proposal_outbound_status: None,
        proposal_event_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
        proposal_app_id: Some(paykit_sdk::PaykitAppId::new("bitkit").unwrap()),
        payer_app_id: Some(paykit_sdk::PaykitAppId::new("wallet").unwrap()),
        execution_claim_app_id: Some(paykit_sdk::PaykitAppId::new("wallet").unwrap()),
        terms: Some(PaymentRequestTermsRecord {
            payment_endpoints: None,
            conversion: None,
            payment_deadline: None,
            amount: AmountRecord {
                value: "10".into(),
                asset: "usd".into(),
            },
            payment_reference: "invoice secret".into(),
            proposal_expires_at: None,
            recurrence: None,
            accepted_payment_endpoint_identifiers: vec!["btc-lightning-bolt11".into()],
            required_app_id: Some(paykit_sdk::PaykitAppId::new("bitkit").unwrap()),
            metadata,
        }),
        accepted_event_id: None,
        accepted_outbound_status: Some(OutboundPrivateMessageStatus::Pending),
        rejected_event_id: None,
        rejected_outbound_status: None,
        canceled_event_id: None,
        canceled_outbound_status: None,
        payment_proofs: vec![PaymentProofRecord {
            conversion_quote_id: None,
            event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
            outbound_message_id: Some(9),
            outbound_status: Some(OutboundPrivateMessageStatus::Sent),
            stream_item_id: None,
            payment_reference: "invoice secret".into(),
            billing_period: Some(BillingPeriodRecord {
                starts_at: "2026-06-01T00:00:00Z".into(),
                ends_at: "2026-07-01T00:00:00Z".into(),
            }),
            payment_app_id: paykit_sdk::PaykitAppId::new("bitkit").unwrap(),
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
    assert_eq!(ffi.payer_app_id.as_deref(), Some("wallet"));
    assert_eq!(ffi.execution_claim_app_id.as_deref(), Some("wallet"));
    assert_eq!(ffi.proposal_app_id.as_deref(), Some("bitkit"));
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
        conversion_quote_id: None,
        billing_period: None,
        payment_app_id: "bitkit".into(),
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
        payment_app_id: "bitkit".into(),

        conversion_quote_id: None,
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
        payment_app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        conversion_quote_id: None,
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
        payment_endpoints: None,
        required_app_id: None,

        amount: FfiPaymentRequestAmount {
            value: "1".into(),
            asset: "btc".into(),
        },
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        proposal_expires_at: None,
        recurrence: None,
        conversion: None,
        payment_deadline: None,
        accepted_payment_endpoint_identifiers: Vec::new(),
        metadata: Arc::new(FfiPrivateJsonObject::new("{}".into()).unwrap()),
    });
    assert!(matches!(result, Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"));
}

#[test]
fn test_conversion_terms_and_quote_selection_survive_bindings() {
    let terms = FfiPaymentRequestTerms {
        payment_endpoints: None,
        required_app_id: None,

        amount: FfiPaymentRequestAmount {
            asset: "usd".into(),
            value: "10".into(),
        },
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        proposal_expires_at: None,
        recurrence: None,
        conversion: Some(FfiPaymentConversion::Fixed {
            rates: vec![FfiConversionRate {
                asset: "usdt-arbitrum".into(),
                value: "1".into(),
            }],
        }),
        payment_deadline: Some(FfiPaymentDeadline::At {
            timestamp: "2026-10-01T12:00:00Z".into(),
        }),
        accepted_payment_endpoint_identifiers: vec!["usdt-arbitrum-address".into()],
        metadata: Arc::new(FfiPrivateJsonObject::new("{}".into()).unwrap()),
    };
    let native = PaymentRequestTerms::try_from(terms).unwrap();
    let restored =
        FfiPaymentRequestTerms::try_from(PaymentRequestTermsRecord::from(&native)).unwrap();
    assert_eq!(PaymentRequestTerms::try_from(restored).unwrap(), native);
    let quote_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103";
    let mut submission = proof_submission(None);
    submission.payment_endpoint_identifier = "usdt-arbitrum-address".into();
    submission.conversion_quote_id = Some(quote_id.into());
    assert_eq!(
        paykit_sdk::PaymentProofSubmission::try_from(submission.clone())
            .unwrap()
            .conversion_quote_id
            .unwrap()
            .as_str(),
        quote_id
    );
    submission.conversion_quote_id = Some("not-a-uuid".into());
    assert!(paykit_sdk::PaymentProofSubmission::try_from(submission).is_err());
    assert_eq!(
        payment_deadline_at(
            FfiPaymentDeadline::PeriodStart { seconds: 86400 },
            Some(FfiBillingPeriod {
                starts_at: "2026-10-01T00:00:00Z".into(),
                ends_at: "2026-11-01T00:00:00Z".into(),
            })
        )
        .unwrap(),
        "2026-10-02T00:00:00Z"
    );
    assert!(payment_deadline_at(FfiPaymentDeadline::PeriodStart { seconds: 86400 }, None).is_err());
}

#[test]
fn test_erc20_receipt_log_index_max_survives_ffi_submission_boundary() {
    let maximum = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let mut submission = proof_submission(None);
    submission.proof = Arc::new(
        FfiPrivateJsonObject::new(format!(
            r#"{{"type":"erc20-transfer-eip712","receipt_log_index":"{maximum}"}}"#
        ))
        .unwrap(),
    );

    let native = paykit_sdk::PaymentProofSubmission::try_from(submission).unwrap();

    assert_eq!(
        native
            .proof
            .get("receipt_log_index")
            .and_then(serde_json::Value::as_str),
        Some(maximum)
    );
}
