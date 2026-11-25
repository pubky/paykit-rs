use paykit_lib::records::{OccurrenceKind, SeenMarks, SeenMerge, SettlementBucket, SettlementMark};
use pubky_dispatch_core::enc::b64;
use pubky_dispatch_core::ident::NamingKey;

const BUCKET: u16 = 17;

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

/// A write of `pairs`, read and bound at bucket [`BUCKET`]'s key.
fn write(pairs: &[(u8, u8)]) -> SettlementBucket {
    let mut marks = marks(pairs);
    marks.sort();
    let bytes: Vec<u8> = marks.iter().flat_map(|m| m.to_bytes()).collect();
    let json = format!(r#"{{"v":1,"marks":"{}"}}"#, b64(&bytes));
    let nk = NamingKey::from_bytes([3; 32]);
    let key = SettlementBucket::record_key(BUCKET, &nk).unwrap();
    SettlementBucket::bind(json.as_bytes(), BUCKET, &nk, &key).unwrap()
}

/// Paykit wire 5.7, Seen marks: what a reader holding seen marks does with each later write.
/// It takes a write that holds them, or a lower mark of their settlement; it merges back over
/// one that lacks them, and shows the writer of one that drops a seen mark or raises it.
#[test]
fn test_settlement_later_write_merges_back_and_shows_its_writer() {
    let mut seen = SeenMarks::new(BUCKET);
    seen.add(&write(&[(1, 1), (2, 2)])).unwrap();
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
    ];
    for (name, later, expected) in cases {
        match (later.merged_with_seen(&seen).unwrap(), expected) {
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
                // The write back holds everything: merging again writes nothing.
                let nk = NamingKey::from_bytes([3; 32]);
                let key = SettlementBucket::record_key(BUCKET, &nk).unwrap();
                let read =
                    SettlementBucket::bind(&bucket.to_json().unwrap(), BUCKET, &nk, &key).unwrap();
                assert_eq!(read, bucket, "{name}");
                assert!(
                    matches!(
                        read.merged_with_seen(&seen).unwrap(),
                        SeenMerge::Holds { .. }
                    ),
                    "{name}"
                );
            }
            (merge, _) => panic!("{name}: {merge:?}"),
        }
    }
}
