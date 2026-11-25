use paykit_lib::records::{
    OccurrenceKind, SeenMarks, SeenMerge, SettlementBucket, SettlementMark, MAX_SETTLEMENT_MARKS,
};
use pubky_dispatch_core::enc::b64;
use pubky_dispatch_core::ident::NamingKey;

const BUCKET: u16 = 17;

/// A value of `marks`, read and bound at bucket [`BUCKET`]'s key.
fn bound(marks: &[SettlementMark]) -> SettlementBucket {
    let mut marks = marks.to_vec();
    marks.sort();
    let bytes: Vec<u8> = marks.iter().flat_map(|m| m.to_bytes()).collect();
    let json = format!(r#"{{"v":1,"marks":"{}"}}"#, b64(&bytes));
    let nk = NamingKey::from_bytes([3; 32]);
    let key = SettlementBucket::record_key(BUCKET, &nk).unwrap();
    SettlementBucket::bind(json.as_bytes(), BUCKET, &nk, &key).unwrap()
}

/// The marks of values that each held one of `marks`, read at bucket [`BUCKET`].
fn seen(marks: &[SettlementMark]) -> SeenMarks {
    let mut seen = SeenMarks::new(BUCKET);
    for mark in marks {
        seen.add(&bound(&[*mark])).unwrap();
    }
    seen
}

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
        SeenMerge::WriteBack { .. } | SeenMerge::OverCapacity { .. } => {
            panic!("the current value already holds the complete lower-mark union")
        }
    }
}

#[test]
fn test_settlement_merge_alerts_lower_current_higher_seen() {
    let current = bound(&[request(1, 1)]);
    let alerts = held_alerts(current.merged_with_seen(&seen(&[request(1, 2)])).unwrap());
    assert_eq!(alerts, vec![request(1, 1).settlement]);
}

#[test]
fn test_settlement_merge_alerts_accepted_lower_replacement() {
    let seen = bound(&[request(1, 2)]);
    let current = bound(&[request(1, 1)]);
    let alerts = held_alerts(current.merged_with(&seen).unwrap());
    assert_eq!(alerts, vec![request(1, 1).settlement]);
}

#[test]
fn test_settlement_merge_alerts_occurrence_kind_tie() {
    let low = mark(1, OccurrenceKind::BillingPeriod, 9);
    let high = mark(1, OccurrenceKind::PaymentWithoutRequest, 0);
    assert!(low < high);
    let current = bound(&[low]);
    assert_eq!(
        held_alerts(current.merged_with_seen(&seen(&[high])).unwrap()),
        vec![low.settlement]
    );
}

#[test]
fn test_settlement_merge_alerts_deduplicate_and_sort_seen_conflicts() {
    let current = bound(&[request(1, 1), request(2, 1)]);
    let alerts = held_alerts(
        current
            .merged_with_seen(&seen(&[
                request(2, 3),
                request(1, 2),
                request(2, 2),
                request(1, 2),
                request(2, 1),
            ]))
            .unwrap(),
    );
    assert_eq!(
        alerts,
        vec![request(1, 1).settlement, request(2, 1).settlement]
    );
}

#[test]
fn test_settlement_merge_alerts_full_bucket_without_new_settlement() {
    let current = bound(
        &(0..MAX_SETTLEMENT_MARKS as u32)
            .map(|i| request(i, 1))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        held_alerts(current.merged_with_seen(&seen(&[request(1, 2)])).unwrap()),
        vec![request(1, 1).settlement]
    );
}

#[test]
fn test_settlement_merge_alerts_identical_seen_marks_are_not_conflicts() {
    let current = bound(&[request(1, 1)]);
    assert!(held_alerts(current.merged_with_seen(&seen(&[])).unwrap()).is_empty());
    assert!(held_alerts(
        current
            .merged_with_seen(&seen(&[request(1, 1); 3]))
            .unwrap()
    )
    .is_empty());
}

#[test]
fn test_settlement_merge_alerts_lower_seen_mark_still_requires_write_back() {
    let current = bound(&[request(1, 2)]);
    let SeenMerge::WriteBack { bucket, twice, .. } =
        current.merged_with_seen(&seen(&[request(1, 1)])).unwrap()
    else {
        panic!("the lower seen occurrence must be written back")
    };
    assert_eq!(bucket.marks(), &[request(1, 1)]);
    assert_eq!(twice, vec![request(1, 1).settlement]);
}

#[test]
fn test_settlement_merge_alerts_full_result_keeps_conflicts_without_capacity() {
    let current = bound(
        &(0..MAX_SETTLEMENT_MARKS as u32)
            .map(|i| request(i, 1))
            .collect::<Vec<_>>(),
    );
    // The bounded write would equal the current one: nothing to write, the alerts kept. A
    // full write lacking a seen mark that didn't fit isn't shown.
    let SeenMerge::OverCapacity {
        bucket: None,
        twice,
        show_writer: false,
    } = current
        .merged_with_seen(&seen(&[
            request(1, 2),
            request(MAX_SETTLEMENT_MARKS as u32, 1),
        ]))
        .unwrap()
    else {
        panic!("a new settlement cannot fit in the full bucket")
    };
    assert_eq!(twice, vec![request(1, 1).settlement]);
}
