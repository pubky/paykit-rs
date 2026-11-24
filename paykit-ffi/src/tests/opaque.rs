//! Opaque JSON from the host keeps every number as written (Paykit wire 1.1).

use crate::opaque::object;

#[test]
fn test_opaque_object_keeps_numbers_written_back_as_given() {
    let kept = object(
        br#"{"count":7,"debt":-3,"big":18446744073709551615,"rate":1.25,"note":"1e2 18446744073709551617","esc":"a\"1e2","list":[0,{"n":12}]}"#,
    )
    .expect("every number re-encodes as written");
    assert_eq!(kept["big"], serde_json::json!(18446744073709551615u64));
    assert_eq!(kept["note"], "1e2 18446744073709551617");
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
        assert!(
            object(bytes).is_none(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    assert!(object(b"[]").is_none());
    assert!(object(b"{").is_none());
}
