use crate::{PaymentRefusal, RefusalKind, SwitchRefusalKind, WalletError, WalletFailureKind};
use paykit_sdk::{PaykitError, Refusal};
use pubky_switch_client::app::{AppError, Refusal as AppRefusal};

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
        let WalletError::Operation {
            kind,
            kept_batch,
            switch_refusal,
            ..
        } = error.into()
        else {
            panic!("a kept client failure must retain its batch");
        };
        assert_eq!(
            matches!(kind, WalletFailureKind::PublicEndpointChangePending),
            pending
        );
        assert_eq!(kept_batch.as_deref(), Some([7; 16].as_slice()));
        assert!(matches!(
            switch_refusal.unwrap().kind(),
            SwitchRefusalKind::RecoveryRequired { generation: 13 }
        ));
    }
    assert!(matches!(
        WalletError::from(AppError::Refused(AppRefusal::Gone)),
        WalletError::Operation {
            switch_refusal: Some(refusal),
            ..
        } if matches!(refusal.kind(), SwitchRefusalKind::Gone)
    ));
}

#[test]
fn test_unsaved_settlement_warning_preserves_the_service_error_and_kept_id() {
    let answer = pubky_switch_client::ClientError::Switch {
        status: 429,
        code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
        current: serde_json::json!({ "limit": "record_keys" }),
        message: "record slot limit".into(),
        known_version: None,
        retry_after: None,
    };
    let error = PaykitError::SettlementNoticeUnavailable {
        bucket: 176,
        submission: Box::new(AppError::Kept {
            batch_id: [4; 16],
            error: Box::new(AppError::Switch(answer)),
        }),
        storage: Box::new(pubky_switch_client::StoreError::Unavailable.into()),
    };
    assert!(matches!(WalletError::from(error), WalletError::Operation {
        kind: WalletFailureKind::Client, service_code: Some(code), kept_batch: Some(id), detail, ..
    } if code == "quota_exceeded" && id == [4; 16] && detail.contains("bucket 176")));
}
