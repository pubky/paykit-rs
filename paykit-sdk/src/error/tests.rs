use super::*;

#[test]
fn test_paykit_not_found_maps_to_sdk_not_found() {
    let err = PaykitSdkError::from(paykit_lib::PaykitError::NotFound("missing receipt".into()));

    assert!(
        matches!(err, PaykitSdkError::NotFound { context, .. } if context == "missing receipt")
    );
}

#[test]
fn test_invalid_data_source_is_not_folded_into_protocol_string() {
    // Regression guard: `Protocol.context` crosses the FFI boundary verbatim
    // (generated Kotlin/Swift exception messages), while lib-level
    // `InvalidData` sources carry raw parse/decode causes that can embed
    // network data or decrypted plaintext. The conversion must keep only the
    // curated static context label and drop the cause entirely:
    // `PaykitSdkError` derives field-wise `Debug`, so a retained `source`
    // would surface in `format!("{err:?}")` and structured Rust logs even
    // though the FFI conversion never forwards it.
    let sentinel = "SENTINEL_RAW_PARSE_CAUSE";
    let err = PaykitSdkError::from(paykit_lib::PaykitError::InvalidData {
        context: "failed to parse receipt plaintext JSON".into(),
        source: Some(anyhow::anyhow!(
            "invalid type: string \"{sentinel}\", expected u8"
        )),
    });

    let (message, source) = match &err {
        PaykitSdkError::Protocol { context, source } => (context.clone(), source.as_ref()),
        other => panic!("expected Protocol error, got {other:?}"),
    };
    assert_eq!(message, "failed to parse receipt plaintext JSON");
    assert!(
        !message.contains(sentinel),
        "InvalidData source leaked into Protocol string: {message}"
    );
    assert!(
        source.is_none(),
        "InvalidData source must be dropped; derived Debug would render it"
    );
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "InvalidData source leaked into Display/Debug: {rendered}"
    );
}
