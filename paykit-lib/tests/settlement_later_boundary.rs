use paykit_lib::records::{
    OccurrenceKind, SeenMerge, SettlementBoundary, SettlementBucket, SettlementMark,
};

fn mark(settlement: u8, occurrence: u8) -> SettlementMark {
    SettlementMark {
        settlement: [settlement; 12],
        kind: OccurrenceKind::OneTimeRequest,
        occurrence: [occurrence; 8],
    }
}

fn known(deletes: Vec<[u8; 32]>) -> SettlementBoundary {
    SettlementBoundary::known(deletes).unwrap()
}

fn read(bucket: &SettlementBucket) -> SettlementBucket {
    SettlementBucket::read(&bucket.to_json().unwrap()).unwrap()
}

// Paykit wire 5.7, Seen marks: a protective boundary permits missing seen marks,
// which the reader merges back without naming the later writer.
fn check_missing_marks(boundary: SettlementBoundary, partial: bool) {
    let held = SettlementBucket::from_marks(vec![mark(1, 1), mark(2, 2)]).unwrap();
    let mut later = SettlementBucket::with_boundary(boundary.clone());
    if partial {
        later = SettlementBucket::from_marks(vec![mark(2, 2), mark(3, 3)]).unwrap();
        later.lost_to_delete = Some(boundary.clone());
    }
    let later = read(&later);
    held.check_later_write(&later).unwrap();
    let SeenMerge::WriteBack { bucket, twice } = later.merged_with(&held) else {
        panic!("the reader must restore its missing seen marks");
    };
    let expected = if partial {
        vec![mark(1, 1), mark(2, 2), mark(3, 3)]
    } else {
        held.marks().to_vec()
    };
    assert_eq!(bucket.marks(), expected);
    assert_eq!(bucket.lost_to_delete, Some(boundary));
    assert!(twice.is_empty());
    held.check_later_write(&bucket).unwrap();
    later.check_later_write(&bucket).unwrap();
    assert_eq!(read(&bucket), bucket);
}

#[test]
fn test_settlement_later_known_boundary_without_marks_merges_seen_marks() {
    check_missing_marks(known(vec![[7; 32]]), false);
}

#[test]
fn test_settlement_later_unknown_boundary_without_marks_merges_seen_marks() {
    check_missing_marks(SettlementBoundary::unknown(), false);
}

#[test]
fn test_settlement_later_known_boundary_with_partial_marks_merges_seen_marks() {
    check_missing_marks(known(vec![[7; 32]]), true);
}

#[test]
fn test_settlement_later_unknown_boundary_with_partial_marks_merges_seen_marks() {
    check_missing_marks(SettlementBoundary::unknown(), true);
}

#[test]
fn test_settlement_later_boundary_rejects_raised_mark_after_missing_mark() {
    let held = SettlementBucket::from_marks(vec![mark(1, 1), mark(2, 2)]).unwrap();
    for boundary in [known(vec![[7; 32]]), SettlementBoundary::unknown()] {
        let mut later = SettlementBucket::from_marks(vec![mark(2, 3)]).unwrap();
        later.lost_to_delete = Some(boundary.clone());
        let error = held.check_later_write(&read(&later)).unwrap_err();
        assert!(error.to_string().contains("changes a mark seen"), "{error}");

        let mut lower = SettlementBucket::from_marks(vec![mark(2, 1)]).unwrap();
        lower.lost_to_delete = Some(boundary);
        held.check_later_write(&read(&lower)).unwrap();
    }
}

#[test]
fn test_settlement_later_boundary_preserves_earlier_deletes() {
    let mut held = SettlementBucket::from_marks(vec![mark(1, 1)]).unwrap();
    held.lost_to_delete = Some(known(vec![[7; 32], [8; 32]]));
    for boundary in [None, Some(known(vec![[7; 32]])), Some(known(vec![[9; 32]]))] {
        for marks in [vec![mark(1, 1)], Vec::new()] {
            // No marks and no boundary is not a valid wire bucket.
            if marks.is_empty() && boundary.is_none() {
                continue;
            }
            let mut later = if marks.is_empty() {
                SettlementBucket::new()
            } else {
                SettlementBucket::from_marks(marks).unwrap()
            };
            later.lost_to_delete = boundary.clone();
            let error = held.check_later_write(&read(&later)).unwrap_err();
            assert!(
                error.to_string().contains("drops a deletion boundary"),
                "{error}"
            );
        }
    }
    let later = SettlementBucket::with_boundary(known(vec![[7; 32], [8; 32], [9; 32]]));
    held.check_later_write(&read(&later)).unwrap();
}

#[test]
fn test_settlement_later_boundary_cannot_narrow_unknown() {
    let mut held = SettlementBucket::from_marks(vec![mark(1, 1)]).unwrap();
    held.lost_to_delete = Some(SettlementBoundary::unknown());
    for marks in [vec![mark(1, 1)], Vec::new()] {
        let mut later = if marks.is_empty() {
            SettlementBucket::new()
        } else {
            SettlementBucket::from_marks(marks).unwrap()
        };
        later.lost_to_delete = Some(known(vec![[7; 32]]));
        let error = held.check_later_write(&read(&later)).unwrap_err();
        assert!(
            error.to_string().contains("drops a deletion boundary"),
            "{error}"
        );
    }
}

#[test]
fn test_settlement_later_without_boundary_rejects_missing_marks() {
    let held = SettlementBucket::from_marks(vec![mark(1, 1), mark(2, 2)]).unwrap();
    let later = SettlementBucket::from_marks(vec![mark(2, 2)]).unwrap();
    let error = held.check_later_write(&read(&later)).unwrap_err();
    assert!(error.to_string().contains("drops a mark seen"), "{error}");
}
