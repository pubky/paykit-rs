use crate::{PaymentRefusal, RefusalKind, SwitchRefusalKind, WalletError, WalletFailure};
use paykit_sdk::{PaykitError, RailError, Refusal};
use pubky_switch_client::{
    app::{AppError, Refusal as AppRefusal},
    error::ClientError,
    transport::TransportError,
};
use pubky_switch_core::http::ErrorCode;

#[test]
fn test_payment_refusals_keep_policy_evidence_and_the_original_attempt_failure() {
    let failed = PaymentRefusal(Refusal::AttemptFailed(Box::new(Refusal::ScopeMissing(
        "paykit_pay".into(),
    ))));
    assert!(matches!(failed.kind(), RefusalKind::AttemptFailed));
    assert!(matches!(failed.attempted_failure().unwrap().kind(),
        RefusalKind::ScopeMissing { scope } if scope == "paykit_pay"));
    assert!(matches!(
        PaymentRefusal(Refusal::IdentityUnavailable("refresh".into())).kind(),
        RefusalKind::IdentityUnavailable
    ));
    assert!(matches!(
        PaymentRefusal(Refusal::IdentityUnconfirmed).kind(),
        RefusalKind::IdentityUnconfirmed
    ));
    assert!(matches!(
        PaymentRefusal(Refusal::PaymentTimeOutside { paid_at_ms: 42 }).kind(),
        RefusalKind::PaymentTimeOutside { paid_at_ms: 42 }
    ));
    assert!(matches!(
        PaymentRefusal(Refusal::RecordHeld("unreadable".into())).kind(),
        RefusalKind::RecordHeld
    ));
    assert!(matches!(
        PaymentRefusal(Refusal::NotCurrent("removed".into())).kind(),
        RefusalKind::NotCurrent
    ));
    assert!(matches!(
        PaymentRefusal(Refusal::SettlementBucketFull { bucket: 23 }).kind(),
        RefusalKind::SettlementBucketFull { bucket: 23 }
    ));
    for behind in [false, true] {
        assert!(matches!(
            PaymentRefusal(Refusal::WaitingForList {
                device_clock_behind: behind
            })
            .kind(),
            RefusalKind::WaitingForList { device_clock_behind } if device_clock_behind == behind
        ));
    }
    let mut doubt = paykit_sdk::PaymentDoubt::default();
    doubt.obligation = [9; 32];
    doubt.step = Some(4);
    doubt.next_possibly_duplicate = true;
    for prior in [None, Some(Box::new(doubt))] {
        let can_confirm = prior.is_some();
        let refusal = PaymentRefusal(Refusal::MayBePaid {
            reason: "previous payment".into(),
            doubt: prior,
        });
        let RefusalKind::MayBePaid { doubt } = refusal.kind() else {
            panic!("missing prior payment refusal");
        };
        assert_eq!(doubt.is_some(), can_confirm);
        if let Some(doubt) = doubt {
            assert_eq!(doubt.obligation, [9; 32]);
            assert_eq!(doubt.step, Some(4));
            assert!(doubt.next_possibly_duplicate);
        }
    }
}

/// The category and failure of a wallet error, for the categories a host branches on.
pub(super) fn categorized(
    error: WalletError,
) -> Option<(&'static str, std::sync::Arc<WalletFailure>)> {
    Some(match error {
        WalletError::NeedsUser { failure } => ("needs_user", failure),
        WalletError::RetryLater { failure } => ("retry_later", failure),
        WalletError::OutcomeUnknown { failure } => ("outcome_unknown", failure),
        WalletError::NotSent { failure } => ("not_sent", failure),
        WalletError::Permanent { failure } => ("permanent", failure),
        WalletError::Recovery { failure } => ("recovery", failure),
        _ => return None,
    })
}

fn switch_answer(code: ErrorCode, retry_after: Option<u64>) -> AppError {
    AppError::Switch(ClientError::Switch {
        status: code.status(),
        code,
        message: String::new(),
        current: serde_json::Value::Null,
        known_version: None,
        retry_after,
    })
}

#[test]
fn test_wallet_errors_categorize_each_failure_with_a_stable_code() {
    let reason = || String::from("reason");
    let cases: Vec<(PaykitError, &str, &str)> = vec![
        (
            PaykitError::Refused(Refusal::MayBePaid {
                reason: reason(),
                doubt: None,
            }),
            "needs_user",
            "may_be_paid_reconcile",
        ),
        (
            PaykitError::Refused(Refusal::MayBePaid {
                reason: reason(),
                doubt: Some(Box::default()),
            }),
            "needs_user",
            "may_be_paid",
        ),
        (
            PaykitError::Refused(Refusal::PeriodNotStarted),
            "needs_user",
            "period_not_started",
        ),
        (
            PaykitError::Refused(Refusal::IdentityUnconfirmed),
            "needs_user",
            "identity_unconfirmed",
        ),
        (
            PaykitError::Refused(Refusal::NotNow(reason())),
            "needs_user",
            "not_now",
        ),
        (
            PaykitError::Refused(Refusal::PaymentInProgress),
            "retry_later",
            "payment_in_progress",
        ),
        (
            PaykitError::Refused(Refusal::NoDestination(reason())),
            "not_sent",
            "no_destination",
        ),
        (
            PaykitError::Refused(Refusal::Usage(reason())),
            "not_sent",
            "usage",
        ),
        (
            PaykitError::Refused(Refusal::NotFound),
            "permanent",
            "not_found",
        ),
        (
            PaykitError::Refused(Refusal::AlreadyPaid),
            "permanent",
            "already_paid",
        ),
        (
            PaykitError::Refused(Refusal::RecordHeld(reason())),
            "needs_user",
            "record_held",
        ),
        (
            PaykitError::Refused(Refusal::NotCurrent(reason())),
            "recovery",
            "not_current",
        ),
        (
            PaykitError::Client(AppError::Transport(TransportError::Timeout)),
            "retry_later",
            "transport",
        ),
        (
            PaykitError::Client(AppError::ExpiredUnsent),
            "not_sent",
            "expired_unsent",
        ),
        (
            PaykitError::Client(AppError::InvalidData(reason())),
            "permanent",
            "invalid_data",
        ),
        (
            PaykitError::Client(AppError::Refused(AppRefusal::Gone)),
            "recovery",
            "gone",
        ),
        (
            PaykitError::Client(AppError::Refused(AppRefusal::Stopped)),
            "recovery",
            "stopped",
        ),
        (
            PaykitError::Client(AppError::Refused(AppRefusal::NotNow(reason()))),
            "needs_user",
            "client_not_now",
        ),
        (
            PaykitError::Client(AppError::Refused(AppRefusal::Usage(reason()))),
            "not_sent",
            "client_usage",
        ),
        (
            PaykitError::Client(switch_answer(ErrorCode::Unavailable, None)),
            "retry_later",
            "switch_unavailable",
        ),
        (
            PaykitError::Client(switch_answer(ErrorCode::StaleIdentity, None)),
            "retry_later",
            "switch_stale_identity",
        ),
        (
            PaykitError::Client(switch_answer(ErrorCode::CertExpired, None)),
            "recovery",
            "switch_cert_expired",
        ),
        (
            PaykitError::Client(switch_answer(ErrorCode::RecoveryRequired, None)),
            "recovery",
            "switch_recovery_required",
        ),
        (
            PaykitError::Client(AppError::Switch(ClientError::Usage(reason()))),
            "not_sent",
            "client_usage",
        ),
        (
            PaykitError::Rail(RailError::Unavailable(reason())),
            "retry_later",
            "method_unknown",
        ),
        (
            PaykitError::Rail(RailError::Refused(reason())),
            "not_sent",
            "method_not_submitted",
        ),
        (
            PaykitError::Rail(RailError::InvalidSelection(reason())),
            "permanent",
            "method_invalid_selection",
        ),
    ];
    for (error, category, code) in cases {
        let refused = matches!(error, PaykitError::Refused(_));
        let (found, failure) = categorized(error.into()).expect("a categorized failure");
        assert_eq!((found, failure.code.as_str()), (category, code));
        assert_eq!(failure.refusal.is_some(), refused);
    }
    // The enrollment and restore answers keep their own variants.
    assert!(matches!(
        WalletError::from(AppError::Refused(AppRefusal::NotJoined)),
        WalletError::NotJoined
    ));
    assert!(matches!(
        WalletError::from(AppError::Refused(AppRefusal::RestoreQuestionOpen)),
        WalletError::RestoreQuestionOpen
    ));
}

#[test]
fn test_wallet_errors_carry_the_service_code_and_its_retry_time() {
    let (category, failure) =
        categorized(switch_answer(ErrorCode::RateLimited, Some(3)).into()).unwrap();
    assert_eq!(category, "retry_later");
    assert_eq!(failure.code, "switch_rate_limited");
    assert_eq!(failure.service_code.as_deref(), Some("rate_limited"));
    assert_eq!(failure.retry_after_ms, Some(3000));
    let (_, failure) = categorized(AppError::Transport(TransportError::Timeout).into()).unwrap();
    assert_eq!(
        (failure.service_code.clone(), failure.retry_after_ms),
        (None, None)
    );
}

#[test]
fn test_wallet_errors_kept_batch_makes_the_outcome_unknown() {
    let kept = AppError::Kept {
        batch_id: [7; 16],
        error: Box::new(AppError::Transport(TransportError::Timeout)),
    };
    let (category, failure) = categorized(kept.into()).unwrap();
    assert_eq!(
        (category, failure.code.as_str()),
        ("outcome_unknown", "transport")
    );
    assert_eq!(failure.kept_batch.as_deref(), Some([7; 16].as_slice()));
    // Without a kept batch the same failure is only a retry.
    let (category, failure) =
        categorized(AppError::Transport(TransportError::Timeout).into()).unwrap();
    assert_eq!(category, "retry_later");
    assert_eq!(failure.kept_batch, None);
}

#[test]
fn test_kept_switch_refusals_retain_the_original_batch_and_recovery_generation() {
    for pending in [false, true] {
        let kept = AppError::Kept {
            batch_id: [7; 16],
            error: Box::new(AppError::Refused(AppRefusal::RecoveryRequired {
                generation: 13,
            })),
        };
        let error = if pending {
            PaykitError::PublicEndpointChangePending(kept)
        } else {
            PaykitError::Client(kept)
        };
        // A recovery outranks the kept batch, and a pending public endpoint edit says so.
        let (category, failure) = categorized(error.into()).unwrap();
        assert_eq!(category, "recovery");
        let code = if pending {
            "public_endpoint_change_pending"
        } else {
            "recovery_required"
        };
        assert_eq!(failure.code, code);
        assert_eq!(failure.kept_batch.as_deref(), Some([7; 16].as_slice()));
        assert!(matches!(
            failure.switch_refusal.clone().unwrap().kind(),
            SwitchRefusalKind::RecoveryRequired { generation: 13 }
        ));
    }
    let (category, failure) = categorized(AppError::Refused(AppRefusal::Gone).into()).unwrap();
    assert_eq!(category, "recovery");
    assert!(matches!(
        failure.switch_refusal.clone().unwrap().kind(),
        SwitchRefusalKind::Gone
    ));
}

/// One of every refusal kind the bindings show. The match below has no wildcard, so a new
/// kind can't be added without a sample here and a row in the host guide.
fn every_refusal_kind() -> Vec<RefusalKind> {
    use crate::{Amount, RequestId};
    let amount = || Amount {
        asset: "btc".into(),
        value: "7".into(),
    };
    let request = || RequestId {
        payee: vec![1; 32],
        request_hash: vec![2; 32],
    };
    let kinds = vec![
        RefusalKind::HeldAfterRestore { record: Vec::new() },
        RefusalKind::Blocked {
            payee: Vec::new(),
            request: None,
            period: None,
            amount: amount(),
        },
        RefusalKind::MayBePaid { doubt: None },
        RefusalKind::MayBePaid {
            doubt: Some(crate::PriorPaymentDoubt {
                obligation: Vec::new(),
                step: None,
                restored: Vec::new(),
                unknown: Vec::new(),
                orphaned: Vec::new(),
                next_possibly_duplicate: false,
                divergent: false,
                after_distrust: false,
                early_pin: false,
                before_loss: None,
            }),
        },
        RefusalKind::LossChanged { boundary: None },
        RefusalKind::Transfer {
            payee: Vec::new(),
            app_id: Vec::new(),
            endpoint: String::new(),
            payload_hash: Vec::new(),
            requested: amount(),
            amount: None,
            max_fee: None,
        },
        RefusalKind::PricedByRate {
            request: request(),
            period: None,
            source_hash: Vec::new(),
            app_id: Vec::new(),
            endpoint: String::new(),
            payload_hash: Vec::new(),
            requested: amount(),
            detail_message_id: None,
            rate: amount(),
            amount: amount(),
        },
        RefusalKind::SettlementBucketFull { bucket: 0 },
        RefusalKind::ListDeliveryUnknown {
            recipient: Vec::new(),
            generation: 0,
            counter: 0,
            msg_id: String::new(),
            withdrawal: false,
            payable_until_ms: None,
        },
        RefusalKind::Divergent {
            records: Vec::new(),
        },
        RefusalKind::NotFound,
        RefusalKind::ScopeMissing {
            scope: String::new(),
        },
        RefusalKind::Label,
        RefusalKind::Rejected,
        RefusalKind::Cancelled,
        RefusalKind::AllowanceEnded,
        RefusalKind::AllowanceConflicted,
        RefusalKind::CreditHistoryLost,
        RefusalKind::AlreadyPaid,
        RefusalKind::BeingHandledByOtherWallet,
        RefusalKind::DeadlinePassed,
        RefusalKind::Recurring,
        RefusalKind::MaybeCredited,
        RefusalKind::PaymentTimeOutside { paid_at_ms: 0 },
        RefusalKind::Subscribed,
        RefusalKind::NotSubscribed,
        RefusalKind::PeriodNotStarted,
        RefusalKind::ManualOnly,
        RefusalKind::NotAutomatic,
        RefusalKind::AttemptedPeriodsFull,
        RefusalKind::Destination,
        RefusalKind::AttemptFailed,
        RefusalKind::NoDestination,
        RefusalKind::UnsupportedEndpoint,
        RefusalKind::WaitingForList {
            device_clock_behind: false,
        },
        RefusalKind::WaitingForList {
            device_clock_behind: true,
        },
        RefusalKind::IdentityUnavailable,
        RefusalKind::IdentityUnconfirmed,
        RefusalKind::PaymentInProgress,
        RefusalKind::NotCurrent,
        RefusalKind::NotNow,
        RefusalKind::FloorHeld,
        RefusalKind::Usage,
        RefusalKind::RecordHeld,
        RefusalKind::CertificateRefused,
        RefusalKind::SwitchUnavailable,
        RefusalKind::Unknown,
    ];
    for kind in &kinds {
        use RefusalKind as K;
        match kind {
            K::HeldAfterRestore { .. }
            | K::Blocked { .. }
            | K::MayBePaid { .. }
            | K::LossChanged { .. }
            | K::Transfer { .. }
            | K::PricedByRate { .. }
            | K::SettlementBucketFull { .. }
            | K::ListDeliveryUnknown { .. }
            | K::Divergent { .. }
            | K::NotFound
            | K::ScopeMissing { .. }
            | K::Label
            | K::Rejected
            | K::Cancelled
            | K::AllowanceEnded
            | K::AllowanceConflicted
            | K::CreditHistoryLost
            | K::AlreadyPaid
            | K::BeingHandledByOtherWallet
            | K::DeadlinePassed
            | K::Recurring
            | K::MaybeCredited
            | K::PaymentTimeOutside { .. }
            | K::Subscribed
            | K::NotSubscribed
            | K::PeriodNotStarted
            | K::ManualOnly
            | K::NotAutomatic
            | K::AttemptedPeriodsFull
            | K::Destination
            | K::AttemptFailed
            | K::NoDestination
            | K::UnsupportedEndpoint
            | K::WaitingForList { .. }
            | K::IdentityUnavailable
            | K::IdentityUnconfirmed
            | K::PaymentInProgress
            | K::NotCurrent
            | K::NotNow
            | K::FloorHeld
            | K::Usage
            | K::RecordHeld
            | K::CertificateRefused
            | K::SwitchUnavailable
            | K::Unknown => {}
        }
    }
    kinds
}

/// The code and category of one of every failure class the bindings sort, other than the
/// payment refusals.
fn every_other_failure() -> Vec<WalletError> {
    let reason = || String::from("reason");
    let client = |e: ClientError| AppError::Switch(e);
    let mut errors: Vec<PaykitError> = ErrorCode::ALL
        .into_iter()
        .map(|code| PaykitError::Client(switch_answer(code, None)))
        .collect();
    // A full quota by the limit it names (Switch wire 9.3).
    for limit in [
        "inbox",
        "storage",
        "connect_requests",
        "open_attempts",
        "attempts",
    ] {
        let mut answer = switch_answer(ErrorCode::QuotaExceeded, None);
        if let AppError::Switch(ClientError::Switch { current, .. }) = &mut answer {
            *current = serde_json::json!({ "limit": limit });
        }
        errors.push(PaykitError::Client(answer));
    }
    let app = [
        AppError::ExpiredUnsent,
        AppError::Transport(TransportError::Timeout),
        AppError::InvalidData(reason()),
        client(ClientError::Transport(TransportError::Timeout)),
        client(ClientError::DisplayReadPending),
        client(ClientError::Unpinned),
        client(ClientError::InvalidResponse(reason())),
        client(ClientError::Usage(reason())),
        client(ClientError::OwnRecoveryRequired { generation: 1 }),
        client(ClientError::BadServiceSignature { status: 404 }),
        client(ClientError::StaleAnswer { server_time_ms: 0 }),
        client(ClientError::BodyUnread {
            status: 429,
            code: ErrorCode::RateLimited,
            retry_after: None,
        }),
        client(ClientError::BodyOverCap { cap: 1 }),
        client(ClientError::Core(pubky_switch_core::Error::Invalid(
            reason(),
        ))),
        AppError::NotAnsweringAsAccount { since_ms: 0 },
        AppError::Alarm(pubky_switch_client::app::Alarm::Rollback { reason: reason() }),
    ];
    errors.extend(app.into_iter().map(PaykitError::Client));
    // A store failure behind a kept batch may have applied; bare, it keeps its own case.
    for kept in [
        AppError::Store(pubky_switch_client::error::StoreError::Unavailable),
        AppError::Protocol(pubky_switch_client::app::StoreFailure::of_entry(
            pubky_switch_client::error::StoreError::Unavailable,
        )),
    ] {
        errors.push(PaykitError::Client(AppError::Kept {
            batch_id: [9; 16],
            error: Box::new(kept),
        }));
    }
    errors.extend([
        PaykitError::PublicEndpointChangePending(AppError::Transport(TransportError::Timeout)),
        PaykitError::Rail(RailError::Unavailable(reason())),
        PaykitError::Rail(RailError::Refused(reason())),
        PaykitError::Rail(RailError::InvalidSelection(reason())),
        PaykitError::Homeserver(TransportError::Timeout),
        PaykitError::Conflict(reason()),
        PaykitError::Lib(paykit_lib::Error::Invalid(reason())),
        PaykitError::Lib(paykit_lib::Error::Validation(reason())),
    ]);
    errors.into_iter().map(WalletError::from).collect()
}

/// The guide's "Wallet errors" table: each code with the case it comes as.
fn guide_rows() -> Vec<(String, String)> {
    let guide = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/paykit-host-guide.md"
    ));
    let start = guide
        .find("\n## Wallet errors\n")
        .expect("the host guide has a Wallet errors section");
    let section = &guide[start + 1..];
    let end = section[3..]
        .find("\n## ")
        .map_or(section.len(), |at| at + 3);
    section[..end]
        .lines()
        .filter_map(|line| {
            let cells: Vec<_> = line.split('|').map(str::trim).collect();
            let code = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
            // The case table above the codes has two columns.
            if cells.len() == 4 {
                return None;
            }
            // The code table's rows: code, from, case, what the host does, all filled.
            assert_eq!(
                cells.len(),
                6,
                "the row for {code} has another column count"
            );
            assert!(
                cells[2..5].iter().all(|cell| !cell.is_empty()),
                "the row for {code} has an empty column"
            );
            let case = cells[3].trim_matches('`');
            Some((code.to_string(), case.to_string()))
        })
        .collect()
}

#[test]
fn test_wallet_errors_match_the_host_guide_table() {
    use std::collections::BTreeMap;
    let mut produced: BTreeMap<String, &str> = BTreeMap::new();
    let mut add = |code: String, case: &'static str| {
        let earlier = produced.insert(code.clone(), case);
        assert!(
            earlier.is_none_or(|earlier| earlier == case),
            "{code} comes as two cases"
        );
    };
    for kind in every_refusal_kind() {
        let (category, code) = crate::error::refusal_category(&kind);
        add(code.into(), category.case());
    }
    let reason = || String::from("reason");
    // Through the same conversion a host's errors take, short-cuts included.
    for refusal in [
        AppRefusal::Stopped,
        AppRefusal::Left,
        AppRefusal::Gone,
        AppRefusal::RecoveryRequired { generation: 1 },
        AppRefusal::Approval(reason()),
        AppRefusal::NotJoined,
        AppRefusal::RestoreQuestionOpen,
        AppRefusal::NotNow(reason()),
        AppRefusal::Usage(reason()),
        AppRefusal::Certificate {
            protocol: reason(),
            reason: reason(),
        },
        AppRefusal::CarriedTooLarge {
            duty_events: 1,
            owed_calls: 0,
        },
        AppRefusal::WontDeliver(pubky_switch_client::app::WontDeliver::GivenUp),
    ] {
        let enrollment = matches!(
            refusal,
            AppRefusal::NotJoined | AppRefusal::RestoreQuestionOpen
        );
        match categorized(AppError::Refused(refusal).into()) {
            Some((case, failure)) => add(failure.code.clone(), case_name(case)),
            None => assert!(enrollment, "a refusal outside the table"),
        }
    }
    for error in every_other_failure() {
        let (case, failure) = categorized(error).expect("a categorized failure");
        add(failure.code.clone(), case_name(case));
    }
    // No test can build these failures; the mapping's own arms give their codes.
    for (category, code) in crate::error::UNBUILT {
        add(code.into(), category.case());
    }
    // `NotSent` promises nothing took effect, so a host may redo the call: each of these is
    // refused before anything was written, sent or applied.
    let not_sent: Vec<_> = produced
        .iter()
        .filter(|(_, case)| **case == "NotSent")
        .map(|(code, _)| code.as_str())
        .collect();
    assert_eq!(
        not_sent,
        [
            "body_over_cap",                 // not sent: over its call's cap
            "carried_too_large",             // nothing written by the restore
            "client_usage",                  // not sent: breaks a Switch rule
            "conflict",                      // nothing applied: the state changed first
            "destination",                   // refused before the attempt was written
            "expired_unsent",                // retired before its first send
            "invalid_input",                 // the caller's value breaks a Paykit rule
            "label",                         // refused before the request was sent
            "method_not_submitted",          // the method submitted nothing
            "no_destination",                // refused before the attempt was written
            "recurring",                     // refused before the attempt was written
            "switch_conflict",               // refused by the switch, nothing applied
            "switch_not_admitted",           // refused by the recipient's switch, nothing applied
            "switch_quota_connect_requests", // the recipient's pending list is full
            "switch_too_large",              // refused by the switch as over a cap, nothing applied
            "unpinned",                      // not sent without a pinned key
            "unsupported_endpoint",          // nothing written
            "usage",                         // the caller's value breaks a Paykit rule
        ]
    );
    let rows = guide_rows();
    let guide: BTreeMap<_, _> = rows.iter().cloned().collect();
    assert_eq!(guide.len(), rows.len(), "a code has two rows in the guide");
    // An attempt that failed comes as its cause's case, which the table says in words.
    assert_eq!(
        guide.get("attempt_failed").map(String::as_str),
        Some("as its cause")
    );
    produced.remove("attempt_failed");
    let mut guide = guide;
    guide.remove("attempt_failed");
    for (code, case) in &produced {
        assert_eq!(
            guide.get(code).map(String::as_str),
            Some(*case),
            "the guide row for {code}"
        );
    }
    for code in guide.keys() {
        assert!(
            produced.contains_key(code),
            "the guide lists {code}, which nothing gives"
        );
    }
}

/// The case name of a category as `categorized` reports it.
fn case_name(category: &str) -> &'static str {
    match category {
        "needs_user" => "NeedsUser",
        "retry_later" => "RetryLater",
        "outcome_unknown" => "OutcomeUnknown",
        "not_sent" => "NotSent",
        "permanent" => "Permanent",
        "recovery" => "Recovery",
        _ => panic!("no case {category}"),
    }
}

#[test]
fn test_wallet_host_refuses_repeated_endpoints_and_too_many_fraction_digits() {
    use crate::{environment::supported_endpoints, EndpointSupport};
    let support = |identifier: &str, fraction_digits| EndpointSupport {
        identifier: identifier.into(),
        fraction_digits,
    };
    let declared = supported_endpoints(vec![
        support("btc-lightning-bolt11", Some(18)),
        support("btc-onchain", None),
    ])
    .unwrap();
    assert_eq!(declared.len(), 2);
    for refused in [
        vec![
            support("btc-onchain", None),
            support("btc-onchain", Some(8)),
        ],
        vec![support("btc-onchain", Some(19))],
        vec![support("private", None)],
    ] {
        assert!(matches!(
            supported_endpoints(refused),
            Err(WalletError::InvalidInput { .. })
        ));
    }
}

#[test]
fn test_wallet_errors_attempt_failed_comes_as_its_cause() {
    for (cause, case) in [
        (
            Refusal::IdentityUnavailable("refresh".into()),
            "retry_later",
        ),
        (Refusal::Destination("expired".into()), "not_sent"),
        (Refusal::NotCurrent("removed".into()), "recovery"),
        (Refusal::DeadlinePassed, "permanent"),
        (
            Refusal::MayBePaid {
                reason: "earlier payment".into(),
                doubt: Some(Box::default()),
            },
            "needs_user",
        ),
    ] {
        let error = PaykitError::Refused(Refusal::AttemptFailed(Box::new(cause)));
        let (found, failure) = categorized(error.into()).expect("a categorized failure");
        assert_eq!((found, failure.code.as_str()), (case, "attempt_failed"));
        assert!(failure
            .refusal
            .as_ref()
            .is_some_and(|r| r.attempted_failure().is_some()));
    }
}

#[test]
fn test_wallet_errors_full_quota_comes_as_what_frees_it() {
    for (limit, case, code) in [
        (Some("inbox"), "retry_later", "switch_quota_wait"),
        (Some("sender_share"), "retry_later", "switch_quota_wait"),
        (Some("claim_share"), "retry_later", "switch_quota_wait"),
        (
            Some("open_attempts"),
            "needs_user",
            "switch_quota_open_attempts",
        ),
        (Some("record_keys"), "needs_user", "switch_quota_full"),
        (Some("conversations"), "needs_user", "switch_quota_full"),
        (Some("deny"), "needs_user", "switch_quota_full"),
        (Some("invite_codes"), "needs_user", "switch_quota_full"),
        (Some("storage"), "needs_user", "switch_quota_full"),
        (Some("contacts"), "needs_user", "switch_quota_full"),
        (Some("claims"), "retry_later", "switch_quota_wait"),
        (
            Some("connect_requests"),
            "not_sent",
            "switch_quota_connect_requests",
        ),
        (Some("attempts"), "permanent", "switch_quota_final"),
        (
            Some("record_origins"),
            "needs_user",
            "switch_quota_exceeded",
        ),
        (Some("a later limit"), "needs_user", "switch_quota_exceeded"),
        (None, "needs_user", "switch_quota_exceeded"),
    ] {
        let mut answer = switch_answer(ErrorCode::QuotaExceeded, None);
        if let (AppError::Switch(ClientError::Switch { current, .. }), Some(limit)) =
            (&mut answer, limit)
        {
            *current = serde_json::json!({ "limit": limit });
        }
        let (found, failure) = categorized(answer.into()).expect("a categorized failure");
        assert_eq!((found, failure.code.as_str()), (case, code), "{limit:?}");
        assert_eq!(failure.service_code.as_deref(), Some("quota_exceeded"));
        // Only a limit name of Switch wire 9.3 is passed on, never the service's other text.
        let known = limit.filter(|limit| *limit != "a later limit" && *limit != "record_origins");
        assert_eq!(failure.quota_limit().as_deref(), known);
    }
}

#[test]
fn test_wallet_errors_store_failure_keeps_its_kept_batch() {
    use pubky_switch_client::error::StoreError;
    assert!(matches!(
        WalletError::from(AppError::Store(StoreError::Unavailable)),
        WalletError::Storage { .. }
    ));
    let kept = AppError::Kept {
        batch_id: [9; 16],
        error: Box::new(AppError::Store(StoreError::Unavailable)),
    };
    let (case, failure) = categorized(kept.into()).expect("a categorized failure");
    assert_eq!(
        (case, failure.code.as_str()),
        ("outcome_unknown", "store_after_kept")
    );
    assert_eq!(failure.kept_batch.as_deref(), Some([9; 16].as_slice()));
}

#[test]
fn test_wallet_errors_context_carries_no_outside_text() {
    // Text the service, the network, the method, the store or a document can put in an error.
    let sentinel = "SENTINEL-outside-text";
    let text = || String::from(sentinel);
    let mut service = switch_answer(ErrorCode::Unavailable, None);
    if let AppError::Switch(ClientError::Switch { message, .. }) = &mut service {
        *message = text();
    }
    let errors = [
        PaykitError::Client(service),
        PaykitError::Client(AppError::Transport(TransportError::Connect(text()))),
        PaykitError::Client(AppError::InvalidData(text())),
        PaykitError::Client(AppError::Refused(AppRefusal::Usage(text()))),
        PaykitError::Client(AppError::Kept {
            batch_id: [3; 16],
            error: Box::new(AppError::Transport(TransportError::Connect(text()))),
        }),
        PaykitError::PublicEndpointChangePending(AppError::Refused(AppRefusal::NotNow(text()))),
        PaykitError::Refused(Refusal::Usage(text())),
        PaykitError::Refused(Refusal::Destination(text())),
        // The SDK builds these from what the switch or the network said.
        PaykitError::Refused(Refusal::SwitchUnavailable(text())),
        PaykitError::Refused(Refusal::IdentityUnavailable(text())),
        PaykitError::Refused(Refusal::CertificateRefused(text())),
        PaykitError::Refused(Refusal::AttemptFailed(Box::new(Refusal::NotNow(text())))),
        PaykitError::Client(AppError::Alarm(pubky_switch_client::app::Alarm::Rollback {
            reason: text(),
        })),
        PaykitError::Client(AppError::Switch(ClientError::InvalidResponse(text()))),
        PaykitError::Client(AppError::Switch(ClientError::Usage(text()))),
        PaykitError::Client(AppError::Switch(ClientError::Transport(
            TransportError::Connect(text()),
        ))),
        PaykitError::Client(AppError::Refused(AppRefusal::Approval(text()))),
        PaykitError::Client(AppError::Refused(AppRefusal::NotNow(text()))),
        PaykitError::Rail(RailError::Refused(text())),
        PaykitError::Rail(RailError::InvalidSelection(text())),
        PaykitError::Lib(paykit_lib::Error::Validation(text())),
        PaykitError::Rail(RailError::Unavailable(text())),
        PaykitError::Lib(paykit_lib::Error::Invalid(text())),
        PaykitError::Conflict(text()),
        PaykitError::Homeserver(TransportError::Connect(text())),
    ];
    for error in errors {
        let error = WalletError::from(error);
        let (_, failure) = categorized(error.clone()).expect("a categorized failure");
        let code = &failure.code;
        // The context is the final code and the meaning of the case the error comes as.
        let (case, _) = categorized(error.clone()).expect("a categorized failure");
        assert_eq!(failure.context, format!("{code}: {}", meaning_of(case)));
        // Every text a host or the bindings can print: the context, the message, the debug
        // output, and the refusals' reason and debug output.
        let mut shown = vec![
            failure.context.clone(),
            error.to_string(),
            format!("{error:?}"),
            format!("{failure:?}"),
        ];
        if let Some(refusal) = &failure.refusal {
            shown.push(refusal.reason());
            shown.push(format!("{refusal:?}"));
            if let Some(cause) = refusal.attempted_failure() {
                shown.push(cause.reason());
            }
        }
        if let Some(rule) = &failure.switch_refusal {
            shown.push(format!("{rule:?}"));
        }
        for text in shown {
            assert!(!text.contains(sentinel), "{code}: {text}");
        }
    }
}

/// What each case tells the host, as `context` and `reason()` spell it.
fn meaning_of(case: &str) -> &'static str {
    match case {
        "needs_user" => "the user must see this and decide",
        "retry_later" => "a later call can succeed",
        "outcome_unknown" => "the call may have taken effect",
        "not_sent" => "nothing took effect",
        "permanent" => "the answer is final",
        "recovery" => "the wallet stopped for a recovery",
        _ => panic!("no case {case}"),
    }
}

#[test]
fn test_wallet_errors_pending_endpoint_change_keeps_what_its_cause_needs() {
    for (inner, case) in [
        (AppError::Transport(TransportError::Timeout), "retry_later"),
        (
            AppError::Refused(AppRefusal::Usage("expiry".into())),
            "needs_user",
        ),
        (
            AppError::Refused(AppRefusal::WontDeliver(
                pubky_switch_client::app::WontDeliver::GivenUp,
            )),
            "needs_user",
        ),
        (AppError::Refused(AppRefusal::Gone), "recovery"),
        (
            AppError::Kept {
                batch_id: [4; 16],
                error: Box::new(AppError::Transport(TransportError::Timeout)),
            },
            "outcome_unknown",
        ),
    ] {
        let (found, failure) =
            categorized(PaykitError::PublicEndpointChangePending(inner).into()).unwrap();
        assert_eq!(
            (found, failure.code.as_str()),
            (case, "public_endpoint_change_pending")
        );
    }
}

#[test]
fn test_wallet_errors_sdk_refusals_take_the_table_code_through_the_conversion() {
    // The guide's table is built from the refusal kinds; the conversion a host's errors take
    // must land on the same code and case.
    for (refusal, case, code) in [
        (Refusal::FloorHeld, "needs_user", "floor_held"),
        (Refusal::NotFound, "permanent", "not_found"),
        (Refusal::Rejected, "permanent", "rejected"),
        (
            Refusal::RecordHeld("unreadable".into()),
            "needs_user",
            "record_held",
        ),
        (
            Refusal::NotCurrent("removed".into()),
            "recovery",
            "not_current",
        ),
        (
            Refusal::SwitchUnavailable("down".into()),
            "retry_later",
            "switch_unavailable",
        ),
        (Refusal::Usage("bad".into()), "not_sent", "usage"),
        (Refusal::NotNow("wait".into()), "needs_user", "not_now"),
    ] {
        let (found, failure) = categorized(PaykitError::Refused(refusal).into()).unwrap();
        assert_eq!((found, failure.code.as_str()), (case, code));
    }
}
