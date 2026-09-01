use chrono::{DateTime, Utc};
use paykit_sdk::{
    AllowanceAmountRangeRecord, AllowanceFilter, AllowanceHistoryStatus, AllowanceLifecycleState,
    AllowanceLocalRole, AllowancePeriodLimitRecord, AllowancePeriodRecord, AllowanceRecord,
    AllowanceTermsRecord, OutboundPrivateMessageStatus, PaykitReceiverPath, PubkyPublicKey,
};

use super::*;

const ALLOWANCE_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";

fn public_key() -> PubkyPublicKey {
    crate::session::parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into())
        .unwrap()
}

fn period_limit(kind: &str, unit: &str, anchor: Option<&str>) -> FfiAllowancePeriodLimit {
    FfiAllowancePeriodLimit {
        amount_limit: Some("100".into()),
        payment_count_limit: Some(4),
        period: FfiAllowancePeriod {
            kind: kind.into(),
            every: 1,
            unit: unit.into(),
            anchor: anchor.map(str::to_owned),
        },
    }
}

#[test]
fn test_allowance_terms_preserve_validated_platform_shape() {
    let terms = FfiAllowanceTerms::new(
        "btc".into(),
        Some(FfiAllowanceAmountRange {
            minimum: "0.1".into(),
            maximum: "1.00".into(),
        }),
        vec![
            period_limit("anchored", "month", Some("2026-01-31T00:00:00Z")),
            period_limit("rolling", "day", None),
        ],
        Some("1000".into()),
        Some("2026-01-01T00:00:00Z".into()),
        Some("2027-01-01T00:00:00Z".into()),
        Some(vec!["btc-lightning-bolt11".into()]),
    )
    .unwrap();

    assert_eq!(terms.asset(), "btc");
    assert_eq!(
        terms.per_payment_amount(),
        Some(FfiAllowanceAmountRange {
            minimum: "0.1".into(),
            maximum: "1.00".into(),
        })
    );
    let limits = terms.period_limits();
    assert_eq!(limits[0].amount_limit.as_deref(), Some("100"));
    assert_eq!(limits[0].payment_count_limit, Some(4));
    assert_eq!(limits[0].period.kind, "anchored");
    assert_eq!(limits[0].period.unit, "month");
    assert_eq!(
        limits[0].period.anchor.as_deref(),
        Some("2026-01-31T00:00:00Z")
    );
    assert_eq!(limits[1].period.kind, "rolling");
    assert_eq!(limits[1].period.unit, "day");
    assert_eq!(limits[1].period.anchor, None);
    assert_eq!(terms.lifetime_amount_limit().as_deref(), Some("1000"));
    assert_eq!(terms.active_from().as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(terms.expires_at().as_deref(), Some("2027-01-01T00:00:00Z"));
    assert_eq!(
        terms.allowed_payment_endpoint_identifiers(),
        Some(vec!["btc-lightning-bolt11".into()])
    );
}

#[test]
fn test_allowance_terms_empty_allowlist_is_not_treated_as_absent() {
    let result = FfiAllowanceTerms::new(
        "btc".into(),
        None,
        Vec::new(),
        Some("1".into()),
        None,
        None,
        Some(Vec::new()),
    );

    assert!(matches!(
        result,
        Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
    ));
}

#[test]
fn test_allowance_terms_reject_anchored_period_without_anchor() {
    let result = FfiAllowanceTerms::new(
        "btc".into(),
        None,
        vec![period_limit("anchored", "month", None)],
        None,
        None,
        None,
        None,
    );

    assert!(matches!(
        result,
        Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
    ));
}

#[test]
fn test_allowance_filter_parses_every_supported_constraint() {
    let counterparty = public_key();
    let filter = FfiAllowanceFilter {
        counterparty: Some(counterparty.to_app_key()),
        counterparty_receiver_path: Some("bitkit/wallet".into()),
        local_role: Some(FfiAllowanceLocalRole::Allowee),
        states: vec![
            FfiAllowanceLifecycleState::Proposed,
            FfiAllowanceLifecycleState::Ended,
        ],
    };

    let parsed = AllowanceFilter::try_from(filter).unwrap();

    assert_eq!(
        parsed,
        AllowanceFilter {
            counterparty: Some(counterparty),
            counterparty_receiver_path: Some(PaykitReceiverPath::new("bitkit/wallet").unwrap()),
            local_role: Some(AllowanceLocalRole::Allowee),
            states: vec![
                AllowanceLifecycleState::Proposed,
                AllowanceLifecycleState::Ended,
            ],
        }
    );
}

#[test]
fn test_allowance_enum_conversions_preserve_known_values_and_reject_unknown_inputs() {
    for (sdk, ffi) in [
        (AllowanceLocalRole::Allower, FfiAllowanceLocalRole::Allower),
        (AllowanceLocalRole::Allowee, FfiAllowanceLocalRole::Allowee),
    ] {
        assert_eq!(FfiAllowanceLocalRole::from(sdk), ffi);
        assert_eq!(AllowanceLocalRole::try_from(ffi).unwrap(), sdk);
    }

    for (sdk, ffi) in [
        (
            AllowanceLifecycleState::Proposed,
            FfiAllowanceLifecycleState::Proposed,
        ),
        (
            AllowanceLifecycleState::Accepted,
            FfiAllowanceLifecycleState::Accepted,
        ),
        (
            AllowanceLifecycleState::Rejected,
            FfiAllowanceLifecycleState::Rejected,
        ),
        (
            AllowanceLifecycleState::Ended,
            FfiAllowanceLifecycleState::Ended,
        ),
        (
            AllowanceLifecycleState::Conflicted,
            FfiAllowanceLifecycleState::Conflicted,
        ),
    ] {
        assert_eq!(FfiAllowanceLifecycleState::from(sdk), ffi);
        assert_eq!(AllowanceLifecycleState::try_from(ffi).unwrap(), sdk);
    }

    for (sdk, ffi) in [
        (
            AllowanceHistoryStatus::Consistent,
            FfiAllowanceHistoryStatus::Consistent,
        ),
        (
            AllowanceHistoryStatus::UnresolvedReferences,
            FfiAllowanceHistoryStatus::UnresolvedReferences,
        ),
        (
            AllowanceHistoryStatus::Invalid,
            FfiAllowanceHistoryStatus::Invalid,
        ),
        (
            AllowanceHistoryStatus::RecoveryRequired,
            FfiAllowanceHistoryStatus::RecoveryRequired,
        ),
    ] {
        assert_eq!(FfiAllowanceHistoryStatus::from(sdk), ffi);
    }

    for filter in [
        FfiAllowanceFilter {
            local_role: Some(FfiAllowanceLocalRole::Unknown),
            ..FfiAllowanceFilter::default()
        },
        FfiAllowanceFilter {
            states: vec![FfiAllowanceLifecycleState::Unknown],
            ..FfiAllowanceFilter::default()
        },
    ] {
        assert!(matches!(
            AllowanceFilter::try_from(filter),
            Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
        ));
    }
}

#[test]
fn test_allowance_id_rejects_invalid_platform_input() {
    assert!(matches!(
        super::conversions::parse_allowance_id("not-an-allowance-id".into()),
        Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
    ));
}

#[test]
fn test_allowance_record_conversion_preserves_lifecycle_evidence() {
    let record = AllowanceRecord {
        counterparty: public_key(),
        counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        allowance_id: ALLOWANCE_ID.into(),
        local_role: Some(AllowanceLocalRole::Allower),
        state: AllowanceLifecycleState::Conflicted,
        history_status: AllowanceHistoryStatus::RecoveryRequired,
        proposal_event_id: Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201".into()),
        terms: Some(AllowanceTermsRecord {
            asset: "btc".into(),
            per_payment_amount: Some(AllowanceAmountRangeRecord {
                minimum: "0.1".into(),
                maximum: "1".into(),
            }),
            period_limits: vec![AllowancePeriodLimitRecord {
                amount_limit: Some("10".into()),
                payment_count_limit: Some(5),
                period: AllowancePeriodRecord {
                    kind: "rolling".into(),
                    every: 1,
                    unit: "day".into(),
                    anchor: None,
                },
            }],
            lifetime_amount_limit: Some("100".into()),
            active_from: None,
            expires_at: None,
            allowed_payment_endpoint_identifiers: None,
        }),
        proposal_stream_item_id: Some(3),
        proposal_outbound_message_id: Some(4),
        proposal_outbound_status: Some(OutboundPrivateMessageStatus::Sent),
        acceptance_event_id: Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202".into()),
        acceptance_outbound_status: Some(OutboundPrivateMessageStatus::Pending),
        rejection_event_id: Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203".into()),
        rejection_outbound_status: Some(OutboundPrivateMessageStatus::Failed),
        end_event_id: Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204".into()),
        end_outbound_status: Some(OutboundPrivateMessageStatus::RecoveryRequired),
        pending_causal_event_ids: vec!["pending-event".into()],
        conflict_event_ids: vec!["conflict-event".into()],
        last_stream_item_id: Some(7),
        last_outbound_message_id: Some(8),
        last_outbound_status: Some(OutboundPrivateMessageStatus::Invalid),
        last_event_at: Some(
            DateTime::parse_from_rfc3339("2026-09-01T12:34:56Z")
                .unwrap()
                .with_timezone(&Utc),
        ),
        invalid_reason: Some("fixed SDK reason".into()),
    };

    let ffi = FfiAllowanceRecord::try_from(record).unwrap();

    assert_eq!(ffi.counterparty, public_key().to_app_key());
    assert_eq!(ffi.counterparty_receiver_path, "bitkit/wallet");
    assert_eq!(ffi.allowance_id, ALLOWANCE_ID);
    assert_eq!(ffi.local_role, Some(FfiAllowanceLocalRole::Allower));
    assert_eq!(ffi.state, FfiAllowanceLifecycleState::Conflicted);
    assert_eq!(
        ffi.history_status,
        FfiAllowanceHistoryStatus::RecoveryRequired
    );
    assert_eq!(
        ffi.proposal_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201")
    );
    assert_eq!(ffi.proposal_stream_item_id, Some(3));
    assert_eq!(ffi.proposal_outbound_message_id, Some(4));
    assert_eq!(
        ffi.proposal_outbound_status,
        Some(FfiOutboundPrivateMessageStatus::Sent)
    );
    assert_eq!(
        ffi.acceptance_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202")
    );
    assert_eq!(
        ffi.acceptance_outbound_status,
        Some(FfiOutboundPrivateMessageStatus::Pending)
    );
    assert_eq!(
        ffi.rejection_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203")
    );
    assert_eq!(
        ffi.rejection_outbound_status,
        Some(FfiOutboundPrivateMessageStatus::Failed)
    );
    assert_eq!(
        ffi.end_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204")
    );
    assert_eq!(
        ffi.end_outbound_status,
        Some(FfiOutboundPrivateMessageStatus::RecoveryRequired)
    );
    assert_eq!(ffi.pending_causal_event_ids, vec!["pending-event"]);
    assert_eq!(ffi.conflict_event_ids, vec!["conflict-event"]);
    assert_eq!(ffi.last_stream_item_id, Some(7));
    assert_eq!(ffi.last_outbound_message_id, Some(8));
    assert_eq!(
        ffi.last_outbound_status,
        Some(FfiOutboundPrivateMessageStatus::Invalid)
    );
    assert_eq!(
        ffi.last_event_at.as_deref(),
        Some("2026-09-01T12:34:56+00:00")
    );
    assert_eq!(ffi.invalid_reason.as_deref(), Some("fixed SDK reason"));
    assert_eq!(ffi.terms.unwrap().asset(), "btc");
}

#[test]
fn test_allowance_terms_debug_redacts_private_fields() {
    let range = FfiAllowanceAmountRange {
        minimum: "123.45".into(),
        maximum: "678.90".into(),
    };
    let limit = period_limit("anchored", "month", Some("2026-02-03T04:05:06Z"));
    let terms = FfiAllowanceTerms::new(
        "private-asset".into(),
        Some(range.clone()),
        vec![limit.clone()],
        Some("987654".into()),
        None,
        None,
        None,
    )
    .unwrap();

    assert!(!format!("{terms:?}").contains("private-asset"));
    assert!(!format!("{terms:?}").contains("987654"));
    assert!(!format!("{range:?}").contains("123.45"));
    assert!(!format!("{:?}", limit.period).contains("2026-02-03T04:05:06Z"));
    assert!(!format!("{limit:?}").contains("100"));
}
