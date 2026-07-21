use super::*;

#[test]
fn test_normalize_receiving_details_rejects_invalid_identifier() {
    let result = normalize_receiving_details(vec![PublicReceivingDetail {
        identifier: "../bad".into(),
        payload: "payload".into(),
    }]);

    assert!(result.is_err());
}

#[test]
fn test_normalize_receiving_details_rejects_duplicates() {
    let result = normalize_receiving_details(vec![
        PublicReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "one".into(),
        },
        PublicReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "two".into(),
        },
    ]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
