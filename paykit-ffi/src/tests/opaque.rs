//! Opaque JSON from the host is checked as the wire checks it and keeps every number as
//! written (Paykit wire 1.1).

use crate::opaque::{object, Place};

fn places() -> [Place<'static>; 3] {
    [
        Place::Metadata,
        Place::ExtensionBody {
            kind: "paykit-cap.offer",
        },
        Place::Proof,
    ]
}

#[test]
fn test_opaque_object_keeps_numbers_written_back_as_given() {
    let bytes = br#"{"count":7,"debt":-3,"big":18446744073709551615,"rate":1.25,"note":"1e2 18446744073709551617","esc":"a\"1e2","list":[0,{"n":12}]}"#;
    let kept = object(bytes, Place::Metadata).expect("every number re-encodes as written");
    assert_eq!(kept["big"], serde_json::json!(18446744073709551615u64));
    assert_eq!(kept["note"], "1e2 18446744073709551617");
    assert!(object(bytes, Place::Proof).is_some());
}

#[test]
fn test_opaque_object_refuses_numbers_that_would_change() {
    for bytes in [
        br#"{"n":18446744073709551617}"#.as_slice(),
        br#"{"n":-9223372036854775809}"#,
        br#"{"n":1e2}"#,
        br#"{"n":1.50}"#,
        br#"{"n":-0}"#,
        br#"{"deep":[{"n":1E400}]}"#,
    ] {
        for place in [Place::Metadata, Place::Proof] {
            assert!(
                object(bytes, place).is_none(),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }
    assert!(object(b"[]", Place::Metadata).is_none());
    assert!(object(b"{", Place::Metadata).is_none());
}

#[test]
fn test_opaque_object_checks_the_wire_first_in_every_place() {
    // Nesting past what the decoder reads is refused here, before anything is journaled.
    let deep = format!("{}1{}", r#"{"a":"#.repeat(200), "}".repeat(200));
    for place in places() {
        // Decoding would drop the first of two repeated members; the wire check sees both.
        assert!(object(br#"{"tx":"a","tx":"b"}"#, place).is_none());
    }
    for place in places() {
        assert!(object(deep.as_bytes(), place).is_none());
    }
    for place in places() {
        assert!(object(br#"{"tx":"a"}"#, place).is_some());
    }
}
