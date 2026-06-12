use super::*;

#[test]
fn test_paykit_not_found_maps_to_sdk_not_found() {
    let err = PaykitSdkError::from(paykit_lib::PaykitError::NotFound("missing receipt".into()));

    assert!(matches!(err, PaykitSdkError::NotFound(message) if message == "missing receipt"));
}
