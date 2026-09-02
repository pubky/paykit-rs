use super::*;

// Destructure any PaykitFfiError into (variant_label, code, context) so the
// table-driven assertions below can compare all nine arms uniformly.
fn parts(err: &PaykitFfiError) -> (&'static str, &str, &str) {
    match err {
        PaykitFfiError::ConcurrentUpdate { code, context } => {
            ("concurrent_update", code.as_str(), context.as_str())
        }
        PaykitFfiError::Storage { code, context } => ("storage", code.as_str(), context.as_str()),
        PaykitFfiError::Identity { code, context } => ("identity", code.as_str(), context.as_str()),
        PaykitFfiError::Transport { code, context } => {
            ("transport", code.as_str(), context.as_str())
        }
        PaykitFfiError::NotFound { code, context } => {
            ("not_found", code.as_str(), context.as_str())
        }
        PaykitFfiError::Protocol { code, context } => ("protocol", code.as_str(), context.as_str()),
        PaykitFfiError::Policy { code, context } => ("policy", code.as_str(), context.as_str()),
        PaykitFfiError::PaymentAdapter { code, context } => {
            ("payment_adapter", code.as_str(), context.as_str())
        }
        PaykitFfiError::RecoveryRequired { code, context } => {
            ("recovery_required", code.as_str(), context.as_str())
        }
    }
}

#[test]
fn test_sdk_error_maps_to_expected_ffi_variant_and_code() {
    // Each SDK variant maps to a stable FFI variant + machine-readable code, with the
    // human-readable context carried through. Source-bearing variants use `source: None`
    // so this exercises the default (non-downcast) mapping path.
    let cases: [(PaykitSdkError, &str, &str, &str); 9] = [
        (
            PaykitSdkError::ConcurrentUpdate {
                context: "shared state changed".into(),
                source: None,
            },
            "concurrent_update",
            "concurrent_update",
            "shared state changed",
        ),
        (
            PaykitSdkError::Storage {
                context: "load state blob".into(),
                source: None,
            },
            "storage",
            "storage_error",
            "load state blob",
        ),
        (
            PaykitSdkError::Identity {
                context: "missing session".into(),
                source: None,
            },
            "identity",
            "identity_error",
            "missing session",
        ),
        (
            PaykitSdkError::Transport {
                context: "peer offline".into(),
                source: None,
            },
            "transport",
            "transport_error",
            "peer offline",
        ),
        (
            PaykitSdkError::NotFound {
                context: "missing receipt".into(),
                source: None,
            },
            "not_found",
            "not_found",
            "missing receipt",
        ),
        (
            PaykitSdkError::Protocol {
                context: "malformed wire".into(),
                source: None,
            },
            "protocol",
            "protocol_error",
            "malformed wire",
        ),
        (
            PaykitSdkError::Policy {
                context: "blocked by policy".into(),
                source: None,
            },
            "policy",
            "policy_error",
            "blocked by policy",
        ),
        (
            PaykitSdkError::PaymentAdapter {
                context: "adapter declined".into(),
                source: None,
            },
            "payment_adapter",
            "payment_adapter_error",
            "adapter declined",
        ),
        (
            PaykitSdkError::RecoveryRequired {
                context: "run recovery".into(),
                source: None,
            },
            "recovery_required",
            "recovery_required",
            "run recovery",
        ),
    ];

    for (sdk_err, want_variant, want_code, want_context) in cases {
        let ffi = PaykitFfiError::from(sdk_err);
        let (variant, code, context) = parts(&ffi);
        assert_eq!(variant, want_variant, "unexpected FFI variant");
        assert_eq!(code, want_code, "unexpected code for {want_variant}");
        assert_eq!(
            context, want_context,
            "unexpected context for {want_variant}"
        );
    }
}

#[test]
fn test_ffi_sdk_round_trip_preserves_variant_and_code() {
    // Callback errors keep their machine-readable identity while replacing
    // app-provided context with the SDK operation that failed.
    let originals = [
        PaykitFfiError::ConcurrentUpdate {
            code: "stale_revision".into(),
            context: "shared state changed".into(),
        },
        PaykitFfiError::Storage {
            code: "atomic_write_failed".into(),
            context: "state blob write failed".into(),
        },
        PaykitFfiError::Identity {
            code: "no_session".into(),
            context: "session expired".into(),
        },
        PaykitFfiError::Transport {
            code: "offline".into(),
            context: "network unreachable".into(),
        },
        PaykitFfiError::NotFound {
            code: "record_missing".into(),
            context: "no such receipt row".into(),
        },
        PaykitFfiError::Protocol {
            code: "bad_wire".into(),
            context: "unexpected field".into(),
        },
        PaykitFfiError::Policy {
            code: "spend_limit".into(),
            context: "daily cap reached".into(),
        },
        PaykitFfiError::PaymentAdapter {
            code: "declined".into(),
            context: "insufficient funds".into(),
        },
        PaykitFfiError::RecoveryRequired {
            code: "state_diverged".into(),
            context: "manual resync needed".into(),
        },
    ];

    for original in originals {
        let sdk = ffi_error_to_sdk(original.clone(), "round trip");
        let restored = PaykitFfiError::from(sdk);
        let (variant, code, context) = parts(&restored);
        let (expected_variant, expected_code, _) = parts(&original);
        assert_eq!(variant, expected_variant);
        assert_eq!(code, expected_code);
        assert_eq!(context, format!("round trip: {expected_code}"));
    }
}

#[test]
fn test_source_chain_is_not_leaked_into_context() {
    // The `context` field crosses the FFI boundary verbatim into the generated
    // Kotlin/Swift exception message, so the raw anyhow cause chain must NOT be folded
    // into it. Only the redacted outer label survives; the underlying cause is dropped
    // entirely (never logged to any subscriber), never reaching the user-facing message.
    let source = anyhow::anyhow!("disk offline").context("open state file");
    let sdk = PaykitSdkError::Storage {
        context: "load state blob".into(),
        source: Some(source),
    };

    let ffi = PaykitFfiError::from(sdk);
    let (variant, code, context) = parts(&ffi);
    assert_eq!(variant, "storage");
    assert_eq!(code, "storage_error");
    assert_eq!(
        context, "load state blob",
        "context must carry only the redacted outer label"
    );
    assert!(
        !context.contains("open state file"),
        "mid-chain cause leaked into context: {context}"
    );
    assert!(
        !context.contains("disk offline"),
        "root cause leaked into context: {context}"
    );
}

#[test]
fn test_sensitive_source_details_never_reach_context() {
    // Regression guard: recovery-marker request URLs embed a DH-derived PRIVATE storage
    // path, and non-2xx HTTP failures can carry a response body. Neither may reach the
    // FFI `context` (which is rendered into the user-facing Kotlin/Swift exception) nor
    // the error's Display output. We plant sentinels in the anyhow cause chain and assert
    // they appear in neither place.
    const SENTINEL_URL: &str = "https://homeserver.example/pub/paykit/v0/private/SENTINEL_DH_PATH";
    const SENTINEL_BODY: &str = "SENTINEL_RESPONSE_BODY";

    let source =
        anyhow::anyhow!("http 502: {SENTINEL_BODY}").context(format!("GET {SENTINEL_URL} failed"));
    let sdk = PaykitSdkError::Transport {
        context: "publish recovery marker".into(),
        source: Some(source),
    };

    let ffi = PaykitFfiError::from(sdk);
    let (variant, code, context) = parts(&ffi);
    assert_eq!(variant, "transport");
    assert_eq!(code, "transport_error");
    assert_eq!(
        context, "publish recovery marker",
        "context must carry only the redacted outer label"
    );

    for sentinel in [SENTINEL_URL, SENTINEL_BODY] {
        assert!(
            !context.contains(sentinel),
            "sensitive detail leaked into context: {sentinel}"
        );
        let rendered = ffi.to_string();
        assert!(
            !rendered.contains(sentinel),
            "sensitive detail leaked into Display: {rendered}"
        );
    }
}

#[test]
fn test_string_variant_sources_never_reach_context() {
    // NotFound/Protocol/Policy/RecoveryRequired used to be plain-string
    // variants, so no cause chain could smuggle sensitive data across the FFI
    // through them. Now that they carry `source` for round-trip downcast
    // recovery, the FFI conversion is the guard: it must drop `source`
    // entirely, exactly like the other four arms. We plant a private-path
    // sentinel in each variant's cause chain and assert it reaches neither
    // `context` nor `Display`.
    const SENTINEL: &str = "/pub/paykit/v0/private/SENTINEL_DH_PATH/receipts/rcpt-1";

    type MakeSdkError = fn(String, Option<anyhow::Error>) -> PaykitSdkError;
    let cases: [(MakeSdkError, &str, &str); 4] = [
        (
            |context, source| PaykitSdkError::NotFound { context, source },
            "not_found",
            "not_found",
        ),
        (
            |context, source| PaykitSdkError::Protocol { context, source },
            "protocol",
            "protocol_error",
        ),
        (
            |context, source| PaykitSdkError::Policy { context, source },
            "policy",
            "policy_error",
        ),
        (
            |context, source| PaykitSdkError::RecoveryRequired { context, source },
            "recovery_required",
            "recovery_required",
        ),
    ];

    for (make, want_variant, want_code) in cases {
        let source = anyhow::anyhow!("GET {SENTINEL} failed");
        let sdk = make("redacted label".into(), Some(source));

        let ffi = PaykitFfiError::from(sdk);
        let (variant, code, context) = parts(&ffi);
        assert_eq!(variant, want_variant);
        assert_eq!(code, want_code);
        assert_eq!(
            context, "redacted label",
            "context must carry only the redacted outer label"
        );
        let rendered = ffi.to_string();
        assert!(
            !rendered.contains(SENTINEL),
            "private path leaked into Display: {rendered}"
        );
    }
}
