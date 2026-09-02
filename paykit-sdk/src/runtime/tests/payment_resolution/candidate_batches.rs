use super::super::*;
use crate::runtime::payment_resolution::{
    app_preference_rank, filter_private_candidate_batch_for_request,
    filter_private_views_by_authorized_apps, filter_public_candidates_for_request,
    load_public_payment_lists_with_budget, public_app_load_order,
    public_payment_endpoint_resource_limit_failure, public_payment_list_fits_resolution_budget,
    unresolved_public_resolution,
};

fn public_app_ids(count: usize) -> Vec<paykit_lib::PaykitAppId> {
    (0..count)
        .map(|index| paykit_lib::PaykitAppId::new(format!("app-{index:02}")).unwrap())
        .collect()
}

fn public_payment_list(endpoint_count: usize, payload_bytes: usize) -> paykit_lib::PaymentList {
    paykit_lib::PaymentList {
        payment_endpoints: (0..endpoint_count)
            .map(|index| {
                (
                    PaymentEndpointIdentifier::new(format!("endpoint-{index}")).unwrap(),
                    paykit_lib::PaymentEndpointPayload::new("x".repeat(payload_bytes)),
                )
            })
            .collect(),
    }
}

fn payment_request_terms(
    required_app_id: Option<&str>,
    accepted_identifiers: &[&str],
) -> PaymentRequestTermsRecord {
    PaymentRequestTermsRecord {
        amount: crate::AmountRecord {
            value: "0.001".into(),
            asset: "btc".into(),
        },
        payment_reference: "invoice-2026-0001".into(),
        proposal_expires_at: None,
        recurrence: None,
        accepted_payment_endpoint_identifiers: accepted_identifiers
            .iter()
            .map(|identifier| (*identifier).to_owned())
            .collect(),
        required_app_id: required_app_id
            .map(|app_id| paykit_lib::PaykitAppId::new(app_id).unwrap()),
        metadata: serde_json::Map::new(),
    }
}

#[test]
fn test_app_preference_rank_uses_endpoint_then_identity_defaults() {
    let bitkit = paykit_lib::PaykitAppId::new("bitkit").unwrap();
    let server = paykit_lib::PaykitAppId::new("server").unwrap();
    let other = paykit_lib::PaykitAppId::new("other").unwrap();
    let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
    let capabilities = paykit_lib::PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    };
    let mut registry =
        paykit_lib::PaykitAppRegistry::new(Some(pubky::Keypair::random().public_key()));
    for (app_id, name) in [
        (bitkit.clone(), "Bitkit"),
        (server.clone(), "Server"),
        (other.clone(), "Other"),
    ] {
        registry
            .register_app(
                app_id,
                paykit_lib::PaykitApp::new(name, capabilities).unwrap(),
            )
            .unwrap();
    }
    registry.set_default_app(Some(bitkit.clone())).unwrap();
    registry
        .set_default_app_for_endpoint(lightning.clone(), server.clone())
        .unwrap();

    assert_eq!(app_preference_rank(&registry, &server, &lightning), 0);
    assert_eq!(app_preference_rank(&registry, &bitkit, &lightning), 1);
    assert_eq!(app_preference_rank(&registry, &other, &lightning), 2);
    assert_eq!(app_preference_rank(&registry, &bitkit, &onchain), 1);
}

#[test]
fn test_public_app_load_order_prioritizes_configured_defaults() {
    let alpha = paykit_lib::PaykitAppId::new("alpha").unwrap();
    let default = paykit_lib::PaykitAppId::new("z-default").unwrap();
    let endpoint_default = paykit_lib::PaykitAppId::new("y-endpoint").unwrap();
    let capabilities = paykit_lib::PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    };
    let mut registry =
        paykit_lib::PaykitAppRegistry::new(Some(pubky::Keypair::random().public_key()));
    for (app_id, name) in [
        (alpha.clone(), "Alpha"),
        (default.clone(), "Default"),
        (endpoint_default.clone(), "Endpoint Default"),
    ] {
        registry
            .register_app(
                app_id,
                paykit_lib::PaykitApp::new(name, capabilities).unwrap(),
            )
            .unwrap();
    }
    registry.set_default_app(Some(default.clone())).unwrap();
    registry
        .set_default_app_for_endpoint(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            endpoint_default.clone(),
        )
        .unwrap();

    assert_eq!(
        public_app_load_order(&registry, None),
        vec![endpoint_default.clone(), default.clone(), alpha.clone()]
    );
    assert_eq!(
        public_app_load_order(&registry, Some(&alpha)),
        vec![alpha, endpoint_default, default]
    );
}

#[test]
fn test_public_payment_list_budget_is_global_and_inclusive() {
    assert!(public_payment_list_fits_resolution_budget(
        255,
        4 * 1024 * 1024 - 1,
        1,
        1,
    ));
    assert!(!public_payment_list_fits_resolution_budget(256, 0, 1, 1,));
    assert!(!public_payment_list_fits_resolution_budget(
        0,
        4 * 1024 * 1024,
        1,
        1,
    ));

    let app_id = paykit_lib::PaykitAppId::new("late-app").unwrap();
    let failure = public_payment_endpoint_resource_limit_failure(app_id.clone());
    assert_eq!(failure.app_id, app_id);
    assert_eq!(
        failure.kind,
        PublicPaymentEndpointLoadFailureKind::ResourceLimit
    );

    let resolution = unresolved_public_resolution(false, vec![failure], 0);
    assert_eq!(
        resolution.status,
        PublicPaymentResolutionStatus::Unavailable
    );
}

#[tokio::test]
async fn test_public_payment_list_loader_passes_remaining_endpoint_budget_across_apps() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let loaded = load_public_payment_lists_with_budget(public_app_ids(64), {
        let calls = calls.clone();
        move |app_id, max_endpoints, max_payload_bytes| {
            calls
                .lock()
                .unwrap()
                .push((app_id.clone(), max_endpoints, max_payload_bytes));
            async move {
                if app_id.as_str() == "app-00" {
                    Ok(public_payment_list(255, 1))
                } else {
                    Ok(public_payment_list(1, 1))
                }
            }
        }
    })
    .await;

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, 256);
    assert_eq!(calls[1].1, 1);
    assert_eq!(calls[1].2, 4 * 1024 * 1024 - 255);
    assert_eq!(loaded.loaded_app_count, 2);
    assert_eq!(loaded.failures.len(), 62);
    assert!(loaded
        .failures
        .iter()
        .all(|failure| { failure.kind == PublicPaymentEndpointLoadFailureKind::ResourceLimit }));
}

#[tokio::test]
async fn test_public_payment_list_loader_passes_remaining_payload_budget_across_apps() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let loaded = load_public_payment_lists_with_budget(public_app_ids(64), {
        let calls = calls.clone();
        move |app_id, max_endpoints, max_payload_bytes| {
            calls
                .lock()
                .unwrap()
                .push((app_id.clone(), max_endpoints, max_payload_bytes));
            async move {
                if app_id.as_str() == "app-00" {
                    Ok(public_payment_list(63, 64 * 1024))
                } else {
                    Ok(public_payment_list(1, 64 * 1024))
                }
            }
        }
    })
    .await;

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].1, 256 - 63);
    assert_eq!(calls[1].2, 64 * 1024);
    assert_eq!(loaded.loaded_app_count, 2);
    assert_eq!(loaded.failures.len(), 62);
    assert!(loaded
        .failures
        .iter()
        .all(|failure| { failure.kind == PublicPaymentEndpointLoadFailureKind::ResourceLimit }));
}

#[tokio::test]
async fn test_public_payment_list_loader_distinguishes_invalid_data_from_resource_limits() {
    let loaded = load_public_payment_lists_with_budget(public_app_ids(3), {
        move |app_id, _, _| async move {
            if app_id.as_str() == "app-00" {
                Err(paykit_lib::PaykitError::InvalidData {
                    context: "malformed Payment Endpoint".into(),
                    source: None,
                })
            } else {
                Ok(public_payment_list(256, 1))
            }
        }
    })
    .await;

    assert_eq!(loaded.loaded_app_count, 1);
    assert_eq!(loaded.failures.len(), 2);
    assert_eq!(loaded.failures[0].app_id.as_str(), "app-00");
    assert_eq!(
        loaded.failures[0].kind,
        PublicPaymentEndpointLoadFailureKind::InvalidData
    );
    assert_eq!(loaded.failures[1].app_id.as_str(), "app-02");
    assert_eq!(
        loaded.failures[1].kind,
        PublicPaymentEndpointLoadFailureKind::ResourceLimit
    );
}

#[test]
fn test_payable_from_batch_rejects_foreign_candidates() {
    let candidate = private_endpoint_candidate("ln-private");
    let foreign = private_endpoint_candidate("ln-foreign");

    let result = private_payable_from_batch(&[foreign], &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_payable_from_batch_rejects_duplicate_candidates() {
    let candidate = private_endpoint_candidate("ln-private");

    let result = private_payable_from_batch(&[candidate.clone(), candidate.clone()], &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_private_payable_from_batch_preserves_adapter_order() {
    let first = private_endpoint_candidate("ln-first");
    let mut second = first.clone();
    second.payload = "ln-second".into();
    let candidates = vec![first.clone(), second.clone()];

    let result = private_payable_from_batch(&[second.clone(), first.clone()], &candidates).unwrap();

    assert_eq!(result, vec![second, first]);
}

#[test]
fn test_private_candidate_batch_returns_complete_aggregate_after_update() {
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let views = vec![
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            latest_stream_item_id: Some(4),
            payment_endpoints: HashMap::from([("btc".into(), "address".into())]),
            last_refresh_at: Some(FixedClock.now()),
        },
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("tether").unwrap(),
            latest_stream_item_id: Some(7),
            payment_endpoints: HashMap::from([("usdt".into(), "account".into())]),
            last_refresh_at: Some(FixedClock.now()),
        },
    ];

    let batch = private_candidate_batch(&counterparty, &views, None)
        .unwrap()
        .unwrap();

    let candidates = batch.candidates();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].app_id.as_str(), "bitkit");
    assert_eq!(candidates[1].app_id.as_str(), "tether");
    assert!(batch.is_newer_than(Some(4)));
}

#[test]
fn test_private_candidate_batch_advances_for_empty_updated_app_list() {
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let views = vec![
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            latest_stream_item_id: Some(4),
            payment_endpoints: HashMap::from([("btc".into(), "address".into())]),
            last_refresh_at: Some(FixedClock.now()),
        },
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("tether").unwrap(),
            latest_stream_item_id: Some(7),
            payment_endpoints: HashMap::new(),
            last_refresh_at: Some(FixedClock.now()),
        },
    ];

    let batch = private_candidate_batch(&counterparty, &views, Some(4))
        .unwrap()
        .unwrap();

    assert!(!batch.has_candidates());
    assert!(batch.is_newer_than(Some(4)));
    assert!(batch.candidates().is_empty());
}

#[test]
fn test_payment_request_constraints_filter_private_candidates() {
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let views = vec![
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            latest_stream_item_id: Some(6),
            payment_endpoints: HashMap::from([(
                "btc-lightning-bolt11".into(),
                "bitkit-lightning".into(),
            )]),
            last_refresh_at: Some(FixedClock.now()),
        },
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("server").unwrap(),
            latest_stream_item_id: Some(7),
            payment_endpoints: HashMap::from([
                ("btc-bitcoin-p2tr".into(), "server-onchain".into()),
                ("btc-lightning-bolt11".into(), "server-lightning".into()),
            ]),
            last_refresh_at: Some(FixedClock.now()),
        },
    ];
    let mut batch = private_candidate_batch(&counterparty, &views, None)
        .unwrap()
        .unwrap();
    let terms = payment_request_terms(Some("server"), &["btc-lightning-bolt11"]);

    filter_private_candidate_batch_for_request(Some(&mut batch), Some(&terms));

    let candidates = batch.candidates();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].app_id.as_str(), "server");
    assert_eq!(candidates[0].identifier, "btc-lightning-bolt11");
}

#[test]
fn test_payment_request_constraints_filter_public_candidates() {
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let mut candidates = vec![
        PublicPaymentEndpointCandidate {
            counterparty: counterparty.clone(),
            app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            identifier: "btc-lightning-bolt11".into(),
            payload: "bitkit-lightning".into(),
        },
        PublicPaymentEndpointCandidate {
            counterparty,
            app_id: paykit_lib::PaykitAppId::new("server").unwrap(),
            identifier: "btc-bitcoin-p2tr".into(),
            payload: "server-onchain".into(),
        },
    ];
    let terms = payment_request_terms(Some("server"), &["btc-bitcoin-p2tr"]);

    filter_public_candidates_for_request(&mut candidates, Some(&terms));

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].app_id.as_str(), "server");
    assert_eq!(candidates[0].identifier, "btc-bitcoin-p2tr");
}

#[test]
fn test_private_views_follow_authorized_app_membership() {
    let mut views = vec![
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            latest_stream_item_id: Some(4),
            payment_endpoints: HashMap::new(),
            last_refresh_at: Some(FixedClock.now()),
        },
        PrivatePaymentListView {
            app_id: paykit_lib::PaykitAppId::new("server").unwrap(),
            latest_stream_item_id: Some(5),
            payment_endpoints: HashMap::new(),
            last_refresh_at: Some(FixedClock.now()),
        },
    ];
    let authorized = vec![paykit_lib::PaykitAppId::new("bitkit").unwrap()];
    filter_private_views_by_authorized_apps(&mut views, Some(&authorized));

    assert_eq!(views.len(), 1);
    assert_eq!(views[0].app_id.as_str(), "bitkit");
}

#[test]
fn test_private_views_require_authorized_app_cache() {
    let mut views = vec![PrivatePaymentListView {
        app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        latest_stream_item_id: Some(4),
        payment_endpoints: HashMap::new(),
        last_refresh_at: Some(FixedClock.now()),
    }];

    filter_private_views_by_authorized_apps(&mut views, None);

    assert!(views.is_empty());
}

#[test]
fn test_private_views_reject_apps_missing_from_authorized_cache() {
    let mut views = vec![PrivatePaymentListView {
        app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        latest_stream_item_id: Some(4),
        payment_endpoints: HashMap::new(),
        last_refresh_at: Some(FixedClock.now()),
    }];

    let authorized = vec![paykit_lib::PaykitAppId::new("server").unwrap()];
    filter_private_views_by_authorized_apps(&mut views, Some(&authorized));

    assert!(views.is_empty());
}

#[test]
fn test_public_payable_from_batch_preserves_adapter_order() {
    let first = public_endpoint_candidate("ln-first");
    let mut second = first.clone();
    second.payload = "ln-second".into();
    let candidates = vec![first.clone(), second.clone()];

    let result = public_payable_from_batch(&[second.clone(), first.clone()], &candidates).unwrap();

    assert_eq!(result, vec![second, first]);
}
