//! Private Payment List reservations (Paykit spec 8.4.1, Paykit wire 5.8).
//!
//! These formats bind a candidate to its protected record. Reading a value does not
//! establish current authority or prove that its reservation applied.

use pubky_switch_core::enc::{b64, pubky_id, zbase32_encode};
use pubky_switch_core::json::check_uuid_v4;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::error::{ensure, Error, Result};
use crate::json::{check_v1, parse_object, Obj};
use crate::records::{Member, Shape, UnknownMembers};
use crate::ListVersion;

/// The fixed collection name (Paykit wire 5.8).
pub const COLLECTION: &str = "private_list_publications";
/// The complete preserved plaintext bound, before the Switch's four-byte length.
pub const MAX_VALUE_BYTES: usize = 508;
/// Every reservation uses this padded plaintext bucket, including withdrawals.
pub const PADDED_VALUE_BYTES: usize = 512;

/// A publisher's reservation value for one recipient and generation (Paykit wire 5.8).
#[derive(Clone, Debug)]
pub struct PrivateListPublication {
    /// The App ID which must also be the authenticated current writer.
    pub publisher: [u8; 16],
    /// The recipient whose independent counter this reservation consumes.
    pub recipient: [u8; 32],
    /// Verified publishing identity generation and reserved recipient counter.
    pub version: ListVersion,
    /// Exact host grant, or `None` for a withdrawal; always written.
    pub grant_id: Option<[u8; 16]>,
    /// The prepared message's fixed UUID v4.
    pub message_id: String,
    /// Hash of the exact unsigned prepared JSON.
    pub message_hash: [u8; 32],
    /// Unknown record members, retained without truncation.
    pub unknown: UnknownMembers,
}

const SECTION: &str = "Paykit wire 5.8: private_list_publications";
static SHAPE: Shape = Shape {
    name: COLLECTION,
    id: &[],
    carried: false,
    members: &[
        ("v", Member::Leaf),
        ("publisher", Member::Leaf),
        ("recipient", Member::Leaf),
        ("generation", Member::Leaf),
        ("counter", Member::Leaf),
        ("grant_id", Member::Leaf),
        ("message_id", Member::Leaf),
        ("message_hash", Member::Leaf),
    ],
};

#[derive(Serialize)]
struct Written<'a> {
    v: u8,
    publisher: String,
    recipient: String,
    generation: u64,
    counter: u64,
    grant_id: Option<String>,
    message_id: &'a str,
    message_hash: String,
    #[serde(flatten)]
    unknown: BTreeMap<&'a str, &'a Value>,
}

impl PrivateListPublication {
    /// Read the current wire value without granting publication authority.
    ///
    /// # Errors
    ///
    /// Missing or invalid members, duplicate JSON keys, or an oversized complete value.
    pub fn read(json: &[u8]) -> Result<Self> {
        check_size(json)?;
        let map = parse_object(json, SECTION)?;
        let obj = Obj::new(&map, SECTION);
        check_v1(&obj)?;
        ensure(obj.has("grant_id"), "Paykit wire 5.8: grant_id is required")?;
        let message_id = obj.req_str("message_id")?.to_owned();
        check_uuid_v4(&message_id)?;
        Ok(Self {
            publisher: obj.req_app_id("publisher")?,
            recipient: obj.req_pubky("recipient")?,
            version: ListVersion::new(obj.req_int("generation")?, obj.req_int("counter")?)
                .map_err(|error| match error {
                    Error::Validation(why) => Error::Invalid(why),
                    other => other,
                })?,
            grant_id: obj.opt_b64("grant_id")?,
            message_id,
            message_hash: obj.req_b64("message_hash")?,
            unknown: UnknownMembers::read(&map, &SHAPE).0,
        })
    }

    /// Write the compact fixed member order, preserving all unknown members.
    ///
    /// # Errors
    ///
    /// Invalid message identity or a complete value exceeding 508 bytes.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        check_uuid_v4(&self.message_id)?;
        let value = Written {
            v: 1,
            publisher: zbase32_encode(&self.publisher),
            recipient: pubky_id(&self.recipient),
            generation: self.version.generation(),
            counter: self.version.counter(),
            grant_id: self.grant_id.as_ref().map(|id| b64(id)),
            message_id: &self.message_id,
            message_hash: b64(&self.message_hash),
            unknown: self.unknown.root_members(&SHAPE),
        };
        let json = serde_json::to_vec(&value)
            .map_err(|_| Error::Invalid(format!("{SECTION}: cannot write value")))?;
        check_size(&json)?;
        Ok(json)
    }
}

fn check_size(json: &[u8]) -> Result<()> {
    ensure(
        json.len() <= MAX_VALUE_BYTES,
        "Paykit wire 5.8: complete publication value exceeds 508 bytes",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication() -> PrivateListPublication {
        PrivateListPublication {
            publisher: [1; 16],
            recipient: [2; 32],
            version: ListVersion::new(7, 12).unwrap(),
            grant_id: Some([3; 16]),
            message_id: "d0efffb7-191b-4709-b6c7-e343af5500b4".into(),
            message_hash: [4; 32],
            unknown: UnknownMembers::default(),
        }
    }

    #[test]
    fn test_publication_value_preserves_required_null_and_unknown_members() {
        let mut original = publication();
        for grant in [None, Some([3; 16])] {
            original.grant_id = grant;
            let bytes = original.to_json().unwrap();
            let back = PrivateListPublication::read(&bytes).unwrap();
            assert_eq!(back.version, original.version);
            assert_eq!(back.grant_id, grant);
            assert_eq!(back.to_json().unwrap(), bytes);
            assert!(String::from_utf8(bytes.clone())
                .unwrap()
                .contains("\"generation\":7,\"counter\":12"));
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value["future"] = serde_json::json!({"kept":true});
            let with_extra =
                PrivateListPublication::read(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&with_extra.to_json().unwrap())
                    .unwrap(),
                value
            );
            value.as_object_mut().unwrap().remove("grant_id");
            assert!(PrivateListPublication::read(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn test_publication_value_checks_full_size_and_duplicate_members() {
        let bytes = publication().to_json().unwrap();
        let duplicate =
            String::from_utf8(bytes.clone())
                .unwrap()
                .replacen("{", "{\"counter\":1,", 1);
        assert!(PrivateListPublication::read(duplicate.as_bytes()).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["future"] = "x".repeat(MAX_VALUE_BYTES).into();
        assert!(PrivateListPublication::read(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut parsed = PrivateListPublication::read(&bytes).unwrap();
        parsed.message_id = "not-an-id".into();
        assert!(parsed.to_json().is_err());
    }
}
