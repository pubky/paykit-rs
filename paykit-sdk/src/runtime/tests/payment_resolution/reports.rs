use super::super::*;
use crate::runtime::payment_resolution::{merge_outbound_report, merge_receive_report};

#[test]
fn test_merge_outbound_report_preserves_multiple_rounds() {
    let mut report = Some(OutboundPrivateSendReport {
        attempted: vec![1],
        sent: vec![1],
        failed: Vec::new(),
        reservation_cleanup_failures: Vec::new(),
        recovery_marker_failures: Vec::new(),
    });
    merge_outbound_report(
        &mut report,
        OutboundPrivateSendReport {
            attempted: vec![2],
            sent: Vec::new(),
            failed: vec![OutboundPrivateSendFailure {
                outbound_message_id: 2,
                error: "transport failed".into(),
            }],
            reservation_cleanup_failures: Vec::new(),
            recovery_marker_failures: Vec::new(),
        },
    );

    let report = report.unwrap();
    assert_eq!(report.attempted, vec![1, 2]);
    assert_eq!(report.sent, vec![1]);
    assert_eq!(report.failed[0].outbound_message_id, 2);
}

#[test]
fn test_merge_receive_report_preserves_multiple_rounds() {
    let mut report = Some(PrivateStreamIntakeReport {
        receive_batch_id: 1,
        stream_item_ids: vec![10],
        event_conflicts: Vec::new(),
    });
    merge_receive_report(
        &mut report,
        PrivateStreamIntakeReport {
            receive_batch_id: 2,
            stream_item_ids: vec![11],
            event_conflicts: vec![EventIdConflict {
                event_id: "event-1".into(),
                first_stream_item_id: 10,
                conflicting_stream_item_id: 11,
            }],
        },
    );

    let report = report.unwrap();
    assert_eq!(report.receive_batch_id, 1);
    assert_eq!(report.stream_item_ids, vec![10, 11]);
    assert_eq!(report.event_conflicts[0].conflicting_stream_item_id, 11);
}
