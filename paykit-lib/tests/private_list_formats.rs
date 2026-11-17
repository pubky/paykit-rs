//! Public format consumers for Paykit wire 4.13 and 5.8.

use paykit_lib::publications::{PrivateListPublication, MAX_VALUE_BYTES, PADDED_VALUE_BYTES};
use paykit_lib::records::UnknownMembers;
use paykit_lib::ListVersion;
use pubky_switch_core::json::MAX_INT;

fn publication() -> PrivateListPublication {
    PrivateListPublication {
        publisher: [1; 16],
        recipient: [2; 32],
        version: ListVersion::new(7, 12).unwrap(),
        grant_id: None,
        message_id: "d0efffb7-191b-4709-b6c7-e343af5500b4".into(),
        message_hash: [4; 32],
        unknown: UnknownMembers::default(),
    }
}

#[test]
fn test_list_pair_wire_order_and_checked_exhaustion() {
    let maximum = ListVersion::new(7, MAX_INT).unwrap();
    let greater = ListVersion::new(8, 1).unwrap();
    assert!(greater > maximum);
    assert!(maximum.next().is_none());
    assert_eq!(
        greater.to_json().unwrap(),
        br#"{"generation":8,"counter":1}"#
    );
    assert_eq!(
        ListVersion::read(&greater.to_json().unwrap()).unwrap(),
        greater
    );
    assert_eq!(
        ListVersion::read(br#"{"counter":1,"future":{},"generation":8}"#).unwrap(),
        greater
    );
    for bytes in [
        br#"{"generation":1,"generation":2,"counter":1}"#.as_slice(),
        br#"{"generation":1,"counter":1,"counter":2}"#.as_slice(),
        br#"{"generation":1,"counter":0}"#.as_slice(),
        br#"{"generation":1}"#.as_slice(),
        br#"{"generation":1,"counter":9007199254740992}"#.as_slice(),
    ] {
        assert!(ListVersion::read(bytes).is_err());
    }
}

#[test]
fn test_publication_value_fixed_member_order_and_bindings() {
    let value = publication();
    let bytes = value.to_json().unwrap();
    let expected = format!(
        "{{\"v\":1,\"publisher\":\"{}\",\"recipient\":\"{}\",\"generation\":7,\"counter\":12,\"grant_id\":null,\"message_id\":\"{}\",\"message_hash\":\"{}\"}}",
        pubky_switch_core::enc::zbase32_encode(&value.publisher),
        pubky_switch_core::enc::pubky_id(&value.recipient),
        value.message_id,
        pubky_switch_core::enc::b64(&value.message_hash),
    );
    assert_eq!(bytes, expected.as_bytes());
    let read = PrivateListPublication::read(&bytes).unwrap();
    assert_eq!(read.publisher, value.publisher);
    assert_eq!(read.recipient, value.recipient);
    assert_eq!(read.version, value.version);
    assert_eq!(read.message_id, value.message_id);
    assert_eq!(read.message_hash, value.message_hash);
    assert_eq!(read.grant_id, None);
    assert_eq!(read.to_json().unwrap(), bytes);
    assert_eq!(PADDED_VALUE_BYTES, MAX_VALUE_BYTES + 4);
}

#[test]
fn test_publication_value_checks_every_required_member() {
    let original: serde_json::Value =
        serde_json::from_slice(&publication().to_json().unwrap()).unwrap();
    for field in [
        "v",
        "publisher",
        "recipient",
        "generation",
        "counter",
        "grant_id",
        "message_id",
        "message_hash",
    ] {
        let mut missing = original.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            PrivateListPublication::read(&serde_json::to_vec(&missing).unwrap()).is_err(),
            "{field}"
        );
    }
    for (field, bad) in [
        ("v", serde_json::json!(2)),
        ("publisher", serde_json::json!("short")),
        ("recipient", serde_json::json!("short")),
        ("generation", serde_json::json!(0)),
        ("counter", serde_json::json!(MAX_INT + 1)),
        ("grant_id", serde_json::json!("short")),
        ("message_id", serde_json::json!("not-a-uuid")),
        ("message_hash", serde_json::json!("short")),
    ] {
        let mut changed = original.clone();
        changed[field] = bad;
        assert!(
            PrivateListPublication::read(&serde_json::to_vec(&changed).unwrap()).is_err(),
            "{field}"
        );
    }
}

#[test]
fn test_publication_value_preserves_unknown_members_within_the_complete_bound() {
    let value = publication();
    let initial = value.to_json().unwrap();
    let mut object: serde_json::Value = serde_json::from_slice(&initial).unwrap();
    object["future"] = serde_json::json!({"nested":[true,7,"kept"]});
    let expanded = serde_json::to_vec(&object).unwrap();
    let decoded = PrivateListPublication::read(&expanded).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&decoded.to_json().unwrap()).unwrap(),
        object
    );
    let mut padded = initial.clone();
    padded.resize(MAX_VALUE_BYTES, b' ');
    assert!(PrivateListPublication::read(&padded).is_ok());
    padded.push(b' ');
    assert!(PrivateListPublication::read(&padded).is_err());
    object["future"] = serde_json::json!("");
    let empty = serde_json::to_vec(&object).unwrap().len();
    object["future"] = serde_json::json!("x".repeat(MAX_VALUE_BYTES - empty));
    let bounded = serde_json::to_vec(&object).unwrap();
    assert_eq!(bounded.len(), MAX_VALUE_BYTES);
    let mut full = PrivateListPublication::read(&bounded).unwrap();
    assert_eq!(full.to_json().unwrap().len(), MAX_VALUE_BYTES);
    full.grant_id = Some([3; 16]);
    assert!(full.to_json().is_err());
    let mut decoded = decoded;
    decoded.message_id = "not-a-uuid".into();
    assert!(decoded.to_json().is_err());
}
