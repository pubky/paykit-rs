use super::*;

#[test]
fn test_paykit_not_found_maps_to_sdk_not_found() {
    let err = PaykitSdkError::from(paykit_lib::PaykitError::NotFound("missing receipt".into()));

    assert!(matches!(err, PaykitSdkError::NotFound(message) if message == "missing receipt"));
}

#[test]
fn test_invalid_data_source_is_not_folded_into_protocol_string() {
    // Regression guard: `Protocol` is a plain string that crosses the FFI
    // boundary verbatim (generated Kotlin/Swift exception messages), while
    // lib-level `InvalidData` sources carry raw parse/decode causes that can
    // embed network data or decrypted plaintext. The conversion must keep only
    // the curated static context label and drop the source entirely.
    let sentinel = "SENTINEL_RAW_PARSE_CAUSE";
    let err = PaykitSdkError::from(paykit_lib::PaykitError::InvalidData {
        context: "failed to parse receipt plaintext JSON".into(),
        source: Some(anyhow::anyhow!(
            "invalid type: string \"{sentinel}\", expected u8"
        )),
    });

    let message = match &err {
        PaykitSdkError::Protocol(message) => message.clone(),
        other => panic!("expected Protocol error, got {other:?}"),
    };
    assert_eq!(message, "failed to parse receipt plaintext JSON");
    assert!(
        !message.contains(sentinel),
        "InvalidData source leaked into Protocol string: {message}"
    );
    let rendered = err.to_string();
    assert!(
        !rendered.contains(sentinel),
        "InvalidData source leaked into Display: {rendered}"
    );
}
