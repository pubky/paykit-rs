use super::*;

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

#[test]
fn test_endpoint_debug_redacts_payloads() {
    let candidate = PaymentEndpointCandidate {
        counterparty: counterparty(),
        source: PaymentEndpointSource::PrivatePaymentList,
        identifier: "btc-lightning-bolt11".into(),
        payload: "ln-private-secret".into(),
    };
    let selection = PaymentEndpointSelection {
        selected: Some(candidate.clone()),
        evaluations: vec![PaymentEndpointEvaluation {
            candidate,
            compatibility: EndpointCompatibility::Payable,
            priority: Some(0),
        }],
    };
    let target = PaymentTarget {
        payload: "method-specific-target".into(),
    };

    let debug = format!("{selection:?} {target:?}");

    assert!(!debug.contains("ln-private-secret"));
    assert!(!debug.contains("method-specific-target"));
    assert!(debug.contains("<redacted:17 bytes>"));
    assert!(debug.contains("<redacted:22 bytes>"));
}
