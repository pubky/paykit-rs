use super::*;
use paykit_sdk::PaymentAmountContext;
use std::sync::Mutex;

#[derive(Default)]
struct TestPaymentAdapter {
    selected_ids: Vec<String>,
    built_candidate_ids: Arc<Mutex<Vec<String>>>,
}

impl FfiSdkPaymentAdapter for TestPaymentAdapter {
    fn current_public_receiving_details(
        &self,
    ) -> Result<Vec<FfiPublicReceivingDetail>, PaykitFfiError> {
        Ok(vec![FfiPublicReceivingDetail {
            identifier: "btc-mainnet-address".into(),
            payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
        }])
    }

    fn current_private_receiving_details(
        &self,
        _counterparty: String,
    ) -> Result<Vec<FfiPrivateReceivingDetail>, PaykitFfiError> {
        Ok(vec![FfiPrivateReceivingDetail {
            identifier: "btc-mainnet-address".into(),
            payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
        }])
    }

    fn reserve_private_receiving_details(
        &self,
        _counterparty: String,
    ) -> Result<FfiPrivateReceivingDetailReservationResponse, PaykitFfiError> {
        Ok(FfiPrivateReceivingDetailReservationResponse {
            kind: FfiPrivateReceivingDetailReservationResponseKind::UseCurrentReceivingDetails,
            reservations: Vec::new(),
        })
    }

    fn cancel_private_receiving_detail_reservation(
        &self,
        _cancellation: FfiPrivatePaymentEndpointReservationCancellation,
    ) -> Result<(), PaykitFfiError> {
        Ok(())
    }

    fn select_public_payment_endpoint_ids(
        &self,
        request: FfiPublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError> {
        assert!(request.candidates[0].candidate_id.starts_with("candidate-"));
        Ok(self.selected_ids.clone())
    }

    fn build_public_payment_target(
        &self,
        endpoint: FfiPublicPaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError> {
        self.built_candidate_ids
            .lock()
            .unwrap()
            .push(endpoint.candidate_id.clone());
        Ok(FfiPaymentTarget {
            payload: Arc::new(FfiPaymentPayload::new(format!(
                "target:{}",
                endpoint.identifier
            ))),
        })
    }

    fn select_private_payment_endpoint_ids(
        &self,
        request: FfiPrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError> {
        assert!(request.candidates[0].candidate_id.starts_with("candidate-"));
        Ok(self.selected_ids.clone())
    }

    fn build_private_payment_target(
        &self,
        endpoint: FfiPrivatePaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError> {
        self.built_candidate_ids
            .lock()
            .unwrap()
            .push(endpoint.candidate_id.clone());
        Ok(FfiPaymentTarget {
            payload: Arc::new(FfiPaymentPayload::new(format!(
                "target:{}",
                endpoint.identifier
            ))),
        })
    }
}

fn candidate(identifier: &str) -> PublicPaymentEndpointCandidate {
    PublicPaymentEndpointCandidate {
        counterparty: PubkyPublicKey::new("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io")
            .unwrap(),
        app_id: paykit_sdk::PaykitAppId::new("bitkit").unwrap(),
        identifier: identifier.into(),
        payload: format!("payload:{identifier}"),
    }
}

fn private_candidate(identifier: &str) -> PrivatePaymentEndpointCandidate {
    PrivatePaymentEndpointCandidate {
        counterparty: PubkyPublicKey::new("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io")
            .unwrap(),
        app_id: paykit_sdk::PaykitAppId::new("bitkit").unwrap(),
        identifier: identifier.into(),
        payload: format!("payload:{identifier}"),
    }
}

#[tokio::test]
async fn test_select_public_payment_endpoint_ids_maps_back_to_candidates() {
    let candidates = vec![
        candidate("btc-mainnet-address"),
        candidate("btc-mainnet-lnurl"),
    ];
    let adapter = FfiSdkPaymentAdapterAdapter {
        adapter: Arc::new(TestPaymentAdapter {
            selected_ids: vec![public_candidate_id(&candidates[1])],
            built_candidate_ids: Arc::default(),
        }),
    };
    let selected = adapter
        .select_public_payment_endpoints(&PublicPaymentEndpointSelectionRequest {
            counterparty: candidates[0].counterparty.clone(),
            amount: Some(PaymentAmountContext {
                value: "1.00".into(),
                asset: "btc".into(),
            }),
            candidates: candidates.clone(),
        })
        .await
        .unwrap();

    assert_eq!(selected, vec![candidates[1].clone()]);
}

#[tokio::test]
async fn test_select_public_payment_endpoint_ids_rejects_unknown_ids() {
    let adapter = FfiSdkPaymentAdapterAdapter {
        adapter: Arc::new(TestPaymentAdapter {
            selected_ids: vec!["missing".into()],
            built_candidate_ids: Arc::default(),
        }),
    };
    let candidates = vec![candidate("btc-mainnet-address")];
    let err = adapter
        .select_public_payment_endpoints(&PublicPaymentEndpointSelectionRequest {
            counterparty: candidates[0].counterparty.clone(),
            amount: None,
            candidates,
        })
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        paykit_sdk::PaykitSdkError::PaymentAdapter { .. }
    ));
}

#[tokio::test]
async fn test_select_private_payment_endpoint_ids_maps_back_to_candidates() {
    let candidates = vec![
        private_candidate("btc-mainnet-address"),
        private_candidate("btc-mainnet-lnurl"),
    ];
    let adapter = FfiSdkPaymentAdapterAdapter {
        adapter: Arc::new(TestPaymentAdapter {
            selected_ids: vec![private_candidate_id(&candidates[1])],
            built_candidate_ids: Arc::default(),
        }),
    };

    let selected = adapter
        .select_private_payment_endpoints(&PrivatePaymentEndpointSelectionRequest {
            counterparty: candidates[0].counterparty.clone(),
            amount: None,
            candidates: candidates.clone(),
        })
        .await
        .unwrap();

    assert_eq!(selected, vec![candidates[1].clone()]);
}

#[test]
fn test_public_and_private_candidates_have_distinct_ids() {
    let public = candidate("btc-mainnet-address");
    let private = private_candidate("btc-mainnet-address");

    assert_ne!(public_candidate_id(&public), private_candidate_id(&private));
}

#[tokio::test]
async fn test_build_public_payment_target_maps_payload() {
    let built_candidate_ids = Arc::new(Mutex::new(Vec::new()));
    let endpoint = candidate("btc-mainnet-address");
    let expected_id = public_candidate_id(&endpoint);
    let adapter = FfiSdkPaymentAdapterAdapter {
        adapter: Arc::new(TestPaymentAdapter {
            selected_ids: Vec::new(),
            built_candidate_ids: built_candidate_ids.clone(),
        }),
    };
    let target = adapter
        .build_public_payment_target(&endpoint)
        .await
        .unwrap();

    assert_eq!(target.payload, "target:btc-mainnet-address");
    assert_eq!(*built_candidate_ids.lock().unwrap(), vec![expected_id]);
}

#[test]
fn test_reservation_response_rejects_mixed_meaning() {
    let response = FfiPrivateReceivingDetailReservationResponse {
        kind: FfiPrivateReceivingDetailReservationResponseKind::UseCurrentReceivingDetails,
        reservations: vec![FfiPrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: FfiPrivateReceivingDetail {
                identifier: "btc-mainnet-address".into(),
                payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
            },
            expires_at: None,
            attribution: Arc::new(FfiReservationAttribution::new(HashMap::new())),
        }],
    };

    let result: paykit_sdk::Result<Option<Vec<PrivatePaymentEndpointReservation>>> =
        response.try_into();

    assert!(matches!(
        result,
        Err(paykit_sdk::PaykitSdkError::PaymentAdapter { .. })
    ));
}

#[test]
fn test_payment_endpoint_reservation_parses_expiry() {
    let reservation = FfiPrivatePaymentEndpointReservation {
        reservation_id: "reservation-1".into(),
        receiving_detail: FfiPrivateReceivingDetail {
            identifier: "btc-mainnet-address".into(),
            payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
        },
        expires_at: Some("2026-06-18T11:00:00Z".into()),
        attribution: Arc::new(FfiReservationAttribution::new(HashMap::new())),
    };
    let reservation = PrivatePaymentEndpointReservation::try_from(reservation).unwrap();

    assert_eq!(
        reservation.expires_at.unwrap().to_rfc3339(),
        "2026-06-18T11:00:00+00:00"
    );
}

#[test]
fn test_payment_endpoint_reservation_classifies_invalid_expiry_as_adapter_error() {
    let reservation = FfiPrivatePaymentEndpointReservation {
        reservation_id: "reservation-1".into(),
        receiving_detail: FfiPrivateReceivingDetail {
            identifier: "btc-mainnet-address".into(),
            payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
        },
        expires_at: Some("not-a-time".into()),
        attribution: Arc::new(FfiReservationAttribution::new(HashMap::new())),
    };

    let error = PrivatePaymentEndpointReservation::try_from(reservation).unwrap_err();

    assert!(matches!(
        error,
        paykit_sdk::PaykitSdkError::PaymentAdapter { .. }
    ));
}

#[test]
fn test_noop_adapter_reports_unavailable_for_receiving_details() {
    let err = FfiNoopSdkPaymentAdapter
        .current_public_receiving_details()
        .unwrap_err();

    assert!(matches!(
        err,
        PaykitFfiError::PaymentAdapter { code, .. }
            if code == "payment_adapter_unavailable"
    ));
}
