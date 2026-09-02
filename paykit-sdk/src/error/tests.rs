use super::*;
use std::error::Error;

#[test]
fn test_paykit_not_found_maps_to_sdk_not_found() {
    let err = PaykitSdkError::from(paykit_lib::PaykitError::NotFound("missing receipt".into()));

    assert!(
        matches!(err, PaykitSdkError::NotFound { context, .. } if context == "missing receipt")
    );
}

#[test]
fn test_invalid_data_source_is_not_folded_into_protocol_string() {
    // Lib-level parse causes can embed network data or decrypted plaintext.
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
        "InvalidData source must be dropped before it reaches SDK callers"
    );
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "InvalidData source leaked into Display/Debug: {rendered}"
    );
}

#[test]
fn test_debug_redacts_source_without_removing_error_chain() {
    let sentinel = "SENTINEL_PRIVATE_SOURCE";
    let err = PaykitSdkError::Storage {
        context: "failed to commit SDK state".into(),
        source: Some(anyhow::anyhow!(sentinel)),
    };

    assert_eq!(err.to_string(), "storage error: failed to commit SDK state");
    assert_eq!(err.source().unwrap().to_string(), sentinel);

    let debug = format!("{err:?}");
    assert!(debug.contains("Storage"));
    assert!(debug.contains("failed to commit SDK state"));
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(sentinel));
}
