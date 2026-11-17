use paykit_lib::records::{OccurrenceKind, SeenMerge, SettlementBucket, SettlementMark};

fn mark(settlement: u8, occurrence: u8) -> SettlementMark {
    SettlementMark {
        settlement: [settlement; 12],
        kind: OccurrenceKind::OneTimeRequest,
        occurrence: [occurrence; 8],
    }
}

fn marks(pairs: &[(u8, u8)]) -> Vec<SettlementMark> {
    pairs.iter().map(|&(s, o)| mark(s, o)).collect()
}

/// A later write of `pairs`, as a reader reads it.
fn write(pairs: &[(u8, u8)]) -> SettlementBucket {
    let bucket = SettlementBucket::from_marks(marks(pairs)).unwrap();
    SettlementBucket::read(&bucket.to_json().unwrap()).unwrap()
}

/// Paykit wire 5.7, Seen marks: what a reader holding seen marks does with each later write.
/// It takes a write that holds them, or a lower mark of their settlement; it merges back over
/// one that lacks them, and shows the writer of one that drops a seen mark or raises it.
#[test]
fn test_settlement_later_write_merges_back_and_shows_its_writer() {
    let seen = marks(&[(1, 1), (2, 2)]);
    // (case, later write, `None` when taken, else the write back's marks and whether the
    // writer is shown)
    type Expected<'a> = Option<(&'a [(u8, u8)], bool)>;
    let cases: Vec<(&str, SettlementBucket, Expected<'_>)> = vec![
        ("the same write", write(&[(1, 1), (2, 2)]), None),
        (
            "a write with another mark",
            write(&[(1, 1), (2, 2), (3, 3)]),
            None,
        ),
        (
            "a lower mark of a seen settlement",
            write(&[(1, 1), (2, 1)]),
            None,
        ),
        (
            "a dropped mark",
            write(&[(2, 2)]),
            Some((&[(1, 1), (2, 2)], true)),
        ),
        (
            "a dropped mark beside another",
            write(&[(2, 2), (3, 3)]),
            Some((&[(1, 1), (2, 2), (3, 3)], true)),
        ),
        (
            "a raised mark",
            write(&[(1, 1), (2, 3)]),
            Some((&[(1, 1), (2, 2)], true)),
        ),
        (
            "an empty bucket",
            SettlementBucket::new(),
            Some((&[(1, 1), (2, 2)], true)),
        ),
    ];
    for (name, later, expected) in cases {
        match (later.merged_with_seen(&seen), expected) {
            (SeenMerge::Holds { .. }, None) => {}
            (
                SeenMerge::WriteBack {
                    bucket,
                    show_writer,
                    ..
                },
                Some((pairs, shown)),
            ) => {
                assert_eq!(bucket.marks(), marks(pairs), "{name}");
                assert_eq!(show_writer, shown, "{name}");
                let read = SettlementBucket::read(&bucket.to_json().unwrap()).unwrap();
                assert_eq!(read, bucket, "{name}");
                // The write back holds everything: merging again writes nothing.
                assert!(
                    matches!(read.merged_with_seen(&seen), SeenMerge::Holds { .. }),
                    "{name}"
                );
            }
            (merge, _) => panic!("{name}: {merge:?}"),
        }
    }
}
