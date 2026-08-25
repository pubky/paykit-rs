use super::*;

/// Recognizable sentinel standing in for decrypted private-message plaintext.
/// It must never appear in any error's `Display` or `Debug` output; `Debug`
/// covers the full source chain, so a clean `Debug` proves the serde source
/// was dropped on the parse path rather than merely hidden from `Display`.
const SENTINEL: &str = "SENTINEL-9f4c-DO-NOT-PRINT";

fn assert_no_sentinel(err: &PaykitError, case: &str) {
    let display = format!("{err}");
    let debug = format!("{err:?}");
    assert!(
        !display.contains(SENTINEL),
        "sentinel leaked into Display for {case}: {display}"
    );
    assert!(
        !debug.contains(SENTINEL),
        "sentinel leaked into Debug for {case}: {debug}"
    );
    assert!(
        err.private_message_parse_category().is_some(),
        "parse error for {case} must carry a typed redacted category, got: {err:?}"
    );
}

#[test]
fn parse_errors_never_echo_plaintext_sentinels() {
    // 1. Sentinel as a field value causing a serde type mismatch, per parser.
    // serde's error Display embeds the offending value verbatim on type
    // mismatches (`invalid type: string "SENTINEL..."`), so these cases fail
    // if any serde detail survives as context or source.
    let list_json = format!(
        r#"{{"version":"{SENTINEL}","kind":"paykit.private_payment_list","payment_endpoints":{{}}}}"#
    );
    assert_no_sentinel(
        &parse_private_payment_list_json(&list_json).unwrap_err(),
        "Private Payment List type mismatch",
    );

    let access_json = format!(r#"{{"version":"{SENTINEL}"}}"#);
    assert_no_sentinel(
        &parse_receipt_access_json(&access_json).unwrap_err(),
        "Receipt Access type mismatch",
    );

    // Payment Request protocol events route through the public event parser;
    // the wrapper's stored validation error must be exactly a stable redacted
    // category string.
    for kind in [
        "paykit.payment_request",
        "paykit.payment_request_acceptance",
        "paykit.payment_request_rejection",
        "paykit.payment_request_cancellation",
        "paykit.payment_proof",
    ] {
        let message = PrivateApplicationMessage {
            version: None,
            kind: None,
            raw_json: format!(r#"{{"version":"{SENTINEL}","kind":"{kind}"}}"#),
        };
        let parsed = parse_payment_request_event_message(&message)
            .expect("recognized Payment Request kind must be routed");
        assert!(!parsed.is_valid());
        let validation_error = parsed
            .validation_error()
            .expect("malformed event must carry a validation error");
        assert!(
            PrivateMessageParseCategory::parse(validation_error).is_some(),
            "validation error for {kind} must be exactly a category string, got: {validation_error}"
        );
        assert_eq!(
            parsed.parse_category(),
            PrivateMessageParseCategory::parse(validation_error)
        );
        assert!(
            !format!("{parsed:?}").contains(SENTINEL),
            "sentinel leaked into wrapper Debug for {kind}"
        );
    }

    // The Receipt Access event wrapper follows the same contract.
    let message = PrivateApplicationMessage {
        version: None,
        kind: None,
        raw_json: format!(r#"{{"version":"{SENTINEL}","kind":"paykit.receipt_access"}}"#),
    };
    let parsed = parse_receipt_access_event_message(&message)
        .expect("recognized Receipt Access kind must be routed");
    assert!(!parsed.is_valid());
    let validation_error = parsed
        .validation_error()
        .expect("malformed event must carry a validation error");
    assert!(
        PrivateMessageParseCategory::parse(validation_error).is_some(),
        "Receipt Access validation error must be exactly a category string, got: {validation_error}"
    );
    assert!(
        !format!("{parsed:?}").contains(SENTINEL),
        "sentinel leaked into Receipt Access wrapper Debug"
    );

    // 2. Sentinel as an unrecognized kind string.
    let unknown_kind_json =
        format!(r#"{{"version":1,"kind":"{SENTINEL}","payment_endpoints":{{}}}}"#);
    assert_no_sentinel(
        &parse_private_payment_list_json(&unknown_kind_json).unwrap_err(),
        "unrecognized kind string",
    );

    // 3. Sentinel inside an invalid Payment Endpoint Identifier (the slash
    // makes it invalid while keeping the sentinel recognizable).
    let bad_identifier_json = format!(
        r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"{SENTINEL}/x":"ln..."}}}}"#
    );
    assert_no_sentinel(
        &parse_private_payment_list_json(&bad_identifier_json).unwrap_err(),
        "invalid Payment Endpoint Identifier",
    );
}
