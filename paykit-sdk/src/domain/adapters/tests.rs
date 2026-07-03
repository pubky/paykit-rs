use super::*;

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_id() -> PaykitReceiverId {
    PaykitReceiverId::new("bitkit").unwrap()
}

#[test]
fn test_endpoint_debug_redacts_payloads() {
    let candidate = PaymentEndpointCandidate {
        counterparty: counterparty(),
        counterparty_receiver_id: receiver_id(),
        source: PaymentEndpointSource::PrivatePaymentList,
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
