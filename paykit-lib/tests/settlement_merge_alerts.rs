use paykit_lib::records::{
    OccurrenceKind, SeenMerge, SettlementBoundary, SettlementBucket, SettlementMark,
    MAX_SETTLEMENT_MARKS,
};

fn mark(settlement: u32, kind: OccurrenceKind, occurrence: u8) -> SettlementMark {
    let mut tag = [0; 12];
    tag[8..].copy_from_slice(&settlement.to_be_bytes());
    SettlementMark {
        settlement: tag,
        kind,
        occurrence: [occurrence; 8],
    }
}

fn request(settlement: u32, occurrence: u8) -> SettlementMark {
    mark(settlement, OccurrenceKind::OneTimeRequest, occurrence)
}

// A host can display the returned tags without requesting a redundant shared write.
fn held_alerts(result: SeenMerge) -> Vec<[u8; 12]> {
    match result {
        SeenMerge::Holds { twice } => twice,
        SeenMerge::WriteBack { .. } | SeenMerge::Full { .. } => {
            panic!("the current value already holds the complete lower-mark union")
        }
    }
}

fn readable(bucket: &SettlementBucket) -> SettlementBucket {
    SettlementBucket::read(&bucket.to_json().unwrap()).unwrap()
}

#[test]
fn test_settlement_merge_alerts_lower_current_higher_seen() {
    let current = readable(&SettlementBucket::from_marks(vec![request(1, 1)]).unwrap());
    let before = current.to_json().unwrap();
    let alerts = held_alerts(current.merged_with_seen(&[request(1, 2)], None));
    assert_eq!(alerts, vec![request(1, 1).settlement]);
    assert_eq!(current.to_json().unwrap(), before);
    assert_eq!(current.marks(), &[request(1, 1)]);
}

#[test]
fn test_settlement_merge_alerts_accepted_lower_replacement() {
    let seen = readable(&SettlementBucket::from_marks(vec![request(1, 2)]).unwrap());
    let current = readable(&SettlementBucket::from_marks(vec![request(1, 1)]).unwrap());
    seen.check_later_write(&current).unwrap();
    let alerts = held_alerts(current.merged_with(&seen));
    assert_eq!(alerts, vec![request(1, 1).settlement]);
    assert_eq!(current.marks(), &[request(1, 1)]);
    assert_eq!(seen.marks(), &[request(1, 2)]);
}

#[test]
fn test_settlement_merge_alerts_occurrence_kind_tie() {
    let low = mark(1, OccurrenceKind::BillingPeriod, 9);
    let high = mark(1, OccurrenceKind::PaymentWithoutRequest, 0);
    assert!(low < high);
    let current = SettlementBucket::from_marks(vec![low]).unwrap();
    assert_eq!(
        held_alerts(current.merged_with_seen(&[high], None)),
        vec![low.settlement]
    );
    assert_eq!(current.marks(), &[low]);
}

#[test]
fn test_settlement_merge_alerts_deduplicate_and_sort_seen_conflicts() {
    let current = SettlementBucket::from_marks(vec![request(1, 1), request(2, 1)]).unwrap();
    let alerts = held_alerts(current.merged_with_seen(
        &[
            request(2, 3),
            request(1, 2),
            request(2, 2),
            request(1, 2),
            request(2, 1),
        ],
        None,
    ));
    assert_eq!(
        alerts,
        vec![request(1, 1).settlement, request(2, 1).settlement]
    );
}

fn boundary_alerts(boundary: SettlementBoundary) {
    let mut current = SettlementBucket::from_marks(vec![request(1, 1)]).unwrap();
    current.lost_to_delete = Some(boundary.clone());
    let current = readable(&current);
    assert_eq!(
        held_alerts(current.merged_with_seen(&[request(1, 2)], Some(&boundary))),
        vec![request(1, 1).settlement]
    );
    assert_eq!(current.lost_to_delete, Some(boundary));
}

#[test]
fn test_settlement_merge_alerts_known_boundary_needs_no_rewrite() {
    boundary_alerts(SettlementBoundary::known(vec![[7; 32]]).unwrap());
}

#[test]
fn test_settlement_merge_alerts_unknown_boundary_needs_no_rewrite() {
    boundary_alerts(SettlementBoundary::unknown());
}

#[test]
fn test_settlement_merge_alerts_full_bucket_without_new_settlement() {
    let current = SettlementBucket::from_marks(
        (0..MAX_SETTLEMENT_MARKS as u32)
            .map(|i| request(i, 1))
            .collect(),
    )
    .unwrap();
    assert_eq!(
        held_alerts(current.merged_with_seen(&[request(1, 2)], None)),
        vec![request(1, 1).settlement]
    );
    assert_eq!(current.marks().len(), MAX_SETTLEMENT_MARKS);
    assert_eq!(
        current.find(&request(1, 1).settlement),
        Some(&request(1, 1))
    );
}

#[test]
fn test_settlement_merge_alerts_identical_seen_marks_are_not_conflicts() {
    let current = SettlementBucket::from_marks(vec![request(1, 1)]).unwrap();
    assert!(held_alerts(current.merged_with_seen(&[], None)).is_empty());
    assert!(held_alerts(current.merged_with_seen(&[request(1, 1); 3], None)).is_empty());
}

#[test]
fn test_settlement_merge_alerts_lower_seen_mark_still_requires_write_back() {
    let current = SettlementBucket::from_marks(vec![request(1, 2)]).unwrap();
    let SeenMerge::WriteBack { bucket, twice } = current.merged_with_seen(&[request(1, 1)], None)
    else {
        panic!("the lower seen occurrence must be written back")
    };
    assert_eq!(bucket.marks(), &[request(1, 1)]);
    assert_eq!(twice, vec![request(1, 1).settlement]);
}

#[test]
fn test_settlement_merge_alerts_full_result_keeps_conflicts_without_capacity() {
    let current = SettlementBucket::from_marks(
        (0..MAX_SETTLEMENT_MARKS as u32)
            .map(|i| request(i, 1))
            .collect(),
    )
    .unwrap();
    let SeenMerge::Full { bucket, twice } = current.merged_with_seen(
        &[request(1, 2), request(MAX_SETTLEMENT_MARKS as u32, 1)],
        None,
    ) else {
        panic!("a new settlement cannot fit in the full bucket")
    };
    assert_eq!(bucket, current);
    assert_eq!(twice, vec![request(1, 1).settlement]);
}
