use super::*;

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

#[test]
fn test_endpoint_debug_redacts_payloads() {
    let candidate = PrivatePaymentEndpointCandidate {
        counterparty: counterparty(),
        app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        identifier: "btc-lightning-bolt11".into(),
        payload: "ln-private-secret".into(),
    };
    let target = PaymentTarget {
        payload: "method-specific-target".into(),
    };

    let debug = format!("{:?} {target:?}", vec![candidate]);

    assert!(!debug.contains("ln-private-secret"));
    assert!(!debug.contains("method-specific-target"));
    assert!(debug.contains("<redacted:17 bytes>"));
    assert!(debug.contains("<redacted:22 bytes>"));
}
