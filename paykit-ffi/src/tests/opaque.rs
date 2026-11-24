//! Opaque JSON from the host is checked as the wire checks it and keeps every number as
//! written (Paykit wire 1.1).

use crate::opaque::{object, Place};

/// The places where the host's JSON is checked before anything is sent.
fn places() -> [Place<'static>; 2] {
    [
        Place::Metadata,
        Place::ExtensionBody {
            kind: "paykit-cap.offer",
        },
    ]
}

#[test]
fn test_opaque_object_keeps_numbers_written_back_as_given() {
    let bytes = br#"{"count":7,"debt":-3,"big":18446744073709551615,"rate":1.25,"note":"1e2 18446744073709551617","esc":"a\"1e2","list":[0,{"n":12}]}"#;
    let kept = object(bytes, Place::Metadata).expect("every number re-encodes as written");
    assert_eq!(kept["big"], serde_json::json!(18446744073709551615u64));
    assert_eq!(kept["note"], "1e2 18446744073709551617");
}

#[test]
fn test_opaque_object_carries_any_proof_that_reads() {
    // The payment is made: a proof is never refused for its numbers or its members.
    for bytes in [
        br#"{"settlement":"a1","fee":1.50}"#.as_slice(),
        br#"{"settlement":18446744073709551617}"#,
        br#"{"tx":"a","tx":"b"}"#,
    ] {
        assert!(
            object(bytes, Place::Proof).is_some(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    assert!(object(b"[]", Place::Proof).is_none());
    assert!(object(b"{", Place::Proof).is_none());
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
        for place in [
            Place::Metadata,
            Place::ExtensionBody {
                kind: "paykit-cap.offer",
            },
        ] {
            let bytes = if matches!(place, Place::Metadata) {
                bytes.to_vec()
            } else {
                [br#"{"metadata":"#.as_slice(), bytes, b"}"].concat()
            };
            let bytes = bytes.as_slice();
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
fn test_opaque_object_checks_the_wire_first_where_the_host_gives_it() {
    for place in places() {
        // Decoding would drop the first of two repeated members; the wire check sees both.
        assert!(object(br#"{"tx":"a","tx":"b"}"#, place).is_none());
        assert!(object(br#"{"tx":"a"}"#, place).is_some());
    }
}

#[test]
fn test_opaque_object_nesting_is_bounded_by_the_wire_check() {
    let nest = |n: usize| format!("{}1{}", r#"{"a":"#.repeat(n), "}".repeat(n));
    let place = |metadata: bool| {
        if metadata {
            Place::Metadata
        } else {
            Place::ExtensionBody {
                kind: "paykit-cap.offer",
            }
        }
    };
    // The wire check reads the host's JSON inside its message, two levels down for
    // metadata and one for an extension body. Decoding the JSON alone takes one level more
    // than either, so these boundaries hold only with the wire check.
    for (metadata, deepest) in [(true, 125), (false, 126)] {
        assert!(object(nest(deepest).as_bytes(), place(metadata)).is_some());
        assert!(object(nest(deepest + 1).as_bytes(), place(metadata)).is_none());
    }
    assert!(serde_json::from_str::<serde_json::Value>(&nest(127)).is_ok());
}
