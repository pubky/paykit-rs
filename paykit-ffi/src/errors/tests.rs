use super::*;

// Destructure any PaykitFfiError into (variant_label, code, context) so the
// table-driven assertions below can compare all eight arms uniformly.
fn parts(err: &PaykitFfiError) -> (&'static str, &str, &str) {
    match err {
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
    let cases: [(PaykitSdkError, &str, &str, &str); 8] = [
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
            PaykitSdkError::NotFound("missing receipt".into()),
            "not_found",
            "not_found",
            "missing receipt",
        ),
        (
            PaykitSdkError::Protocol("malformed wire".into()),
            "protocol",
            "protocol_error",
            "malformed wire",
        ),
        (
            PaykitSdkError::Policy("blocked by policy".into()),
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
            PaykitSdkError::RecoveryRequired("run recovery".into()),
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
fn test_ffi_sdk_round_trip_recovers_original_via_downcast() {
    // The four source-bearing arms wrap the original FFI error in the SDK error's
    // `source`; converting back must downcast to the exact original (same code +
    // context), not a re-labelled placeholder. Pins F03: without the PaymentAdapter
    // `source` passthrough the adapter case regresses to `payment_adapter_error`.
    let originals = [
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
        PaykitFfiError::PaymentAdapter {
            code: "declined".into(),
            context: "insufficient funds".into(),
        },
    ];

    for original in originals {
        let sdk = ffi_error_to_sdk(original.clone(), "round trip");
        let restored = PaykitFfiError::from(sdk);
        assert_eq!(
            parts(&restored),
            parts(&original),
            "round trip must preserve the original error"
        );
    }
}

#[test]
fn test_source_chain_is_folded_into_context() {
    // A non-FFI source has its full anyhow cause chain folded into the context so foreign
    // apps see the underlying cause. Pins F02: the pre-fix code dropped `source` and only
    // the outer context survived.
    let source = anyhow::anyhow!("disk offline").context("open state file");
    let sdk = PaykitSdkError::Storage {
        context: "load state blob".into(),
        source: Some(source),
    };

    let ffi = PaykitFfiError::from(sdk);
    let (variant, code, context) = parts(&ffi);
    assert_eq!(variant, "storage");
    assert_eq!(code, "storage_error");
    assert!(
        context.contains("load state blob"),
        "outer context missing: {context}"
    );
    assert!(
        context.contains("open state file"),
        "mid-chain cause missing: {context}"
    );
    assert!(
        context.contains("disk offline"),
        "root cause missing: {context}"
    );
}
