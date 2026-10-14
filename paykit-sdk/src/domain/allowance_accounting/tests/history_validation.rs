use super::*;
use std::time::{Duration as Elapsed, Instant};

/// Debug build: linear work measured 4-10x even on an oversubscribed runner, the
/// old quadratic scans about 60x.
const MAX_GROWTH: f64 = 20.0;

/// Two selected requests on one link, the first paid automatically a minute after
/// its selection, followed by `fillers` watermarks for unrelated Allowances.
fn ledger(fillers: usize) -> AllowanceAccountingState {
    let local = public_key();
    let counterparty = public_key();
    let watermark = |allowance_id: &str| AllowanceWatermarkRecord {
        local_public_key: local.clone(),
        counterparty: counterparty.clone(),
        allowance_id: allowance_id.to_owned(),
        evaluated_at: time(),
    };
    let mut history = AllowanceAccountingHistory::default();
    for _ in 0..2 {
        let request = PaymentAccountingScope {
            local_public_key: local.clone(),
            counterparty: counterparty.clone(),
            payment_request_id: paykit_lib::PaymentRequestId::new_v4().as_str().to_owned(),
        };
        let allowance_id = AllowanceId::new_v4().as_str().to_owned();
        history.watermarks.push(watermark(&allowance_id));
        history.associations.push(AllowanceAssociationRecord {
            request: request.clone(),
            revisions: vec![AllowanceAssociationRevision {
                revision: 1,
                allowance_id: allowance_id.clone(),
                effective_from: None,
                authorization_id: None,
                authorized_at: time(),
            }],
        });
        history.occurrences.push(PaymentOccurrenceRecord {
            key: PaymentOccurrenceKey {
                request,
                billing_period: None,
            },
            disposition: PaymentDisposition::Automatic,
            allowance_id: Some(allowance_id),
            association_revision: Some(1),
            attempts: Vec::new(),
        });
    }
    let paid_at = time() + Duration::minutes(1);
    history.watermarks[0].evaluated_at = paid_at;
    let paid = &mut history.occurrences[0];
    paid.attempts.push(PaymentAttemptRecord {
        attempt_id: new_id(),
        mode: PaymentExecutionMode::Automatic,
        allowance_id: paid.allowance_id.clone(),
        association_revision: Some(1),
        amount: crate::AmountRecord {
            value: "1".into(),
            asset: "btc".into(),
        },
        admitted_at: paid_at,
        status: PaymentExecutionStatus::Succeeded,
        epoch: new_id(),
    });
    history
        .watermarks
        .extend((0..fillers).map(|_| watermark(AllowanceId::new_v4().as_str())));
    AllowanceAccountingState {
        revision: 0,
        epoch: new_id(),
        requires_reconciliation: false,
        history,
    }
}

#[track_caller]
fn assert_rejected(mutate: impl FnOnce(&mut AllowanceAccountingHistory), expected: &str) {
    let mut state = ledger(2);
    mutate(&mut state.history);
    match validate_accounting(&state) {
        Err(PaykitSdkError::Protocol { context, .. }) => assert_eq!(context, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

/// Compares 8 runs on `rows` watermarks with one run on 8x as many. The timed
/// windows are equally long (about 30 ms in a debug build), interleaved, and
/// minimized, so a busy runner slows both alike.
fn assert_linear(rows: usize, timed: impl Fn(&AllowanceAccountingState, usize) -> Elapsed) {
    let (small, large) = (ledger(rows), ledger(8 * rows));
    let (mut eight_small, mut one_large) = (Elapsed::MAX, Elapsed::MAX);
    for _ in 0..5 {
        eight_small = eight_small.min(timed(&small, 8));
        one_large = one_large.min(timed(&large, 1));
    }
    let growth = 8.0 * one_large.as_secs_f64() / eight_small.as_secs_f64();
    assert!(
        growth < MAX_GROWTH,
        "8x watermarks took {growth:.1}x as long: {one_large:?} vs {eight_small:?} for 8 runs"
    );
}

#[test]
fn test_accounting_validation_scales_linearly_with_watermarks() {
    assert_linear(2000, |state, runs| {
        let start = Instant::now();
        for _ in 0..runs {
            validate_accounting(state).unwrap();
        }
        start.elapsed()
    });
}

#[test]
fn test_accounting_restore_merge_scales_linearly_with_watermarks() {
    // Restore validates three times, so fewer rows give the same window length.
    assert_linear(500, |state, runs| {
        let inputs: Vec<_> = (0..runs).map(|_| (state.clone(), state.clone())).collect();
        let start = Instant::now();
        let results: Vec<_> = inputs
            .into_iter()
            .map(|(current, restored)| merge_restored_accounting(Some(current), Some(restored)))
            .collect();
        let elapsed = start.elapsed();
        for merged in results {
            let merged = merged.unwrap().unwrap();
            assert_eq!(
                merged.history.watermarks.len(),
                state.history.watermarks.len()
            );
        }
        elapsed
    });
}

#[test]
fn test_accounting_validation_rejects_duplicate_history() {
    assert_rejected(
        |h| h.associations.push(h.associations[1].clone()),
        "Accounting association history is incomplete or duplicated",
    );
    assert_rejected(
        |h| h.occurrences.push(h.occurrences[1].clone()),
        "Duplicate payment occurrence",
    );
    assert_rejected(
        |h| h.watermarks.push(h.watermarks[1].clone()),
        "Duplicate accounting watermark",
    );
}

#[test]
fn test_accounting_validation_rejects_watermark_behind_history() {
    assert_rejected(
        |h| h.watermarks[1].evaluated_at = time() - Duration::seconds(1),
        "Accounting watermark precedes selection history",
    );
    assert_rejected(
        |h| h.watermarks[1].counterparty = public_key(),
        "Accounting watermark precedes selection history",
    );
    assert_rejected(
        |h| h.watermarks[0].evaluated_at = time(),
        "Accounting watermark precedes retained admission",
    );
}

#[test]
fn test_accounting_validation_rejects_unmatched_attribution() {
    let context = "Accounting attribution has no matching association revision";
    assert_rejected(|h| h.occurrences[1].association_revision = Some(2), context);
    assert_rejected(
        |h| h.occurrences[0].attempts[0].association_revision = Some(2),
        context,
    );
    assert_rejected(
        |h| h.occurrences[1].allowance_id = h.occurrences[0].allowance_id.clone(),
        context,
    );
}

#[test]
fn test_accounting_restore_merge_keeps_latest_watermark_per_scope() {
    let current = ledger(2);
    let mut restored = current.clone();
    restored.history.watermarks[1].evaluated_at += Duration::hours(1);
    restored.history.watermarks[2].evaluated_at -= Duration::hours(1);
    restored.history.watermarks.push(AllowanceWatermarkRecord {
        allowance_id: AllowanceId::new_v4().as_str().to_owned(),
        ..current.history.watermarks[3].clone()
    });
    let merged = merge_restored_accounting(Some(current.clone()), Some(restored.clone()))
        .unwrap()
        .unwrap();
    let row = |w: &AllowanceWatermarkRecord| (w.allowance_id.clone(), w.evaluated_at);
    let rows = |state: &AllowanceAccountingState| {
        state.history.watermarks.iter().map(row).collect::<Vec<_>>()
    };
    let mut expected = rows(&current);
    expected[1].1 = restored.history.watermarks[1].evaluated_at;
    expected.push(row(&restored.history.watermarks[4]));
    assert_eq!(rows(&merged), expected);
}
