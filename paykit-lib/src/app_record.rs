//! Paykit's member of an app record, `protocols.paykit` (Paykit spec 4, Paykit wire 3.2).
//!
//! The Switch checks only the form of `protocols`: an object of objects under names of a
//! scope name's syntax (Switch wire 3.2). A Paykit reader then reads Paykit's member here: the
//! Payment Endpoint Identifiers the app can pay to, and its public endpoints. It refuses a
//! record whose member isn't of the form of Paykit wire 3.2, whose identifiers or payloads
//! break Paykit wire 1.4, or that carries public endpoints without `expires_at`, as it refuses
//! any invalid record, though the switch takes it.
//!
//! Security: public endpoints are destinations anyone can read from the Homeserver. This
//! module reads only what a verified record says; before paying to one, a payer still checks
//! the record's expiry and the identity's record age limit, and that the publishing app is
//! current with the certificate the record carries (Paykit spec 4, 7.2).

use std::collections::BTreeMap;

use pubky_switch_core::records::AppRecord;
use serde_json::{Map, Value};

use crate::error::{invalid, Result};
use crate::identifiers::{PaymentEndpointIdentifier, PaymentEndpointPayload};

/// Paykit's protocol name, the name of its member under an app record's `protocols` (Switch
/// wire 3.2 "Protocols' members").
pub const PROTOCOL_NAME: &str = "paykit";

/// What a Paykit reader takes from an app record's Paykit member (Paykit wire 3.2). A missing
/// or `null` member, `capabilities`, `endpoint_identifiers` or `public_endpoints` reads as
/// empty, and unknown members are ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaykitMember {
    /// `capabilities.endpoint_identifiers`: the Payment Endpoint Identifiers the app can pay
    /// to, as listed.
    pub endpoint_identifiers: Vec<PaymentEndpointIdentifier>,
    /// `public_endpoints`: the app's public endpoints, by Payment Endpoint Identifier.
    pub public_endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

impl PaykitMember {
    /// Read the Paykit member of a verified app record (Paykit wire 3.2), with the record's
    /// signed `expires_at`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] naming the rule the member breaks; the message never repeats
    /// an identifier or a payload.
    pub fn read(record: &AppRecord) -> Result<Self> {
        Self::from_member(record.body().protocol(PROTOCOL_NAME), record.expires_at())
    }

    /// Read a Paykit member as an app record carries it, `None` when the record has none,
    /// with the record's `expires_at` (Paykit wire 3.2).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] for a `capabilities` or `public_endpoints` that isn't an
    /// object, an `endpoint_identifiers` that isn't an array of strings, an identifier that
    /// isn't a Payment Endpoint Identifier or a payload that isn't a non-empty string (Paykit
    /// wire 1.4), and public endpoints without `expires_at`.
    pub fn from_member(
        member: Option<&Map<String, Value>>,
        expires_at: Option<u64>,
    ) -> Result<Self> {
        let Some(member) = member else {
            return Ok(Self::default());
        };
        let endpoint_identifiers = match present(member.get("capabilities")) {
            None => Vec::new(),
            Some(Value::Object(capabilities)) => read_identifiers(capabilities)?,
            Some(_) => return invalid("Paykit wire 3.2: capabilities isn't an object"),
        };
        let public_endpoints = match present(member.get("public_endpoints")) {
            None => BTreeMap::new(),
            Some(Value::Object(endpoints)) => read_public_endpoints(endpoints)?,
            Some(_) => return invalid("Paykit wire 3.2: public_endpoints isn't an object"),
        };
        if !public_endpoints.is_empty() && expires_at.is_none() {
            return invalid("Paykit wire 3.2: a record with public endpoints carries expires_at");
        }
        Ok(Self {
            endpoint_identifiers,
            public_endpoints,
        })
    }
}

/// A member's value, `None` when it is missing or `null` (Paykit wire, Conventions).
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}

/// `capabilities.endpoint_identifiers`: an array of Payment Endpoint Identifiers.
fn read_identifiers(capabilities: &Map<String, Value>) -> Result<Vec<PaymentEndpointIdentifier>> {
    let Some(listed) = present(capabilities.get("endpoint_identifiers")) else {
        return Ok(Vec::new());
    };
    let Value::Array(listed) = listed else {
        return invalid("Paykit wire 3.2: endpoint_identifiers isn't an array");
    };
    listed
        .iter()
        .map(|id| match id {
            Value::String(text) => PaymentEndpointIdentifier::parse(text).or_else(|_| {
                invalid(
                    "Paykit wire 3.2: an endpoint identifier in capabilities isn't a Payment \
                     Endpoint Identifier",
                )
            }),
            _ => invalid("Paykit wire 3.2: endpoint_identifiers holds a non-string"),
        })
        .collect()
}

/// `public_endpoints`: Payment Endpoint Identifiers to non-empty payload strings.
fn read_public_endpoints(
    endpoints: &Map<String, Value>,
) -> Result<BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
    endpoints
        .iter()
        .map(|(id, payload)| {
            let id = PaymentEndpointIdentifier::parse(id).or_else(|_| {
                invalid(
                    "Paykit wire 3.2: a public endpoint's key isn't a Payment Endpoint Identifier",
                )
            })?;
            let Value::String(payload) = payload else {
                return invalid("Paykit wire 3.2: a public endpoint's payload isn't a string");
            };
            let payload = PaymentEndpointPayload::parse(payload)
                .or_else(|_| invalid("Paykit wire 3.2: a public endpoint's payload is empty"))?;
            Ok((id, payload))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn member(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            unreachable!()
        };
        map
    }

    const EXPIRES: Option<u64> = Some(1_767_225_600_000);

    #[test]
    fn test_app_record_member_reads_identifiers_and_endpoints() {
        let m = member(json!({
            "capabilities": { "endpoint_identifiers": ["btc-lightning-bolt11"], "future": 1 },
            "public_endpoints": { "btc-lightning-bolt12": "lno1" },
            "future": { "level": 1 }
        }));
        let read = PaykitMember::from_member(Some(&m), EXPIRES).unwrap();
        assert_eq!(
            read.endpoint_identifiers[0].as_str(),
            "btc-lightning-bolt11"
        );
        let (id, payload) = read.public_endpoints.iter().next().unwrap();
        assert_eq!(
            (id.as_str(), payload.as_str()),
            ("btc-lightning-bolt12", "lno1")
        );
    }

    #[test]
    fn test_app_record_member_missing_and_null_read_as_empty() {
        assert_eq!(
            PaykitMember::from_member(None, None).unwrap(),
            PaykitMember::default()
        );
        for m in [
            json!({}),
            json!({ "capabilities": null, "public_endpoints": null }),
            json!({ "capabilities": { "endpoint_identifiers": null }, "public_endpoints": {} }),
        ] {
            let read = PaykitMember::from_member(Some(&member(m.clone())), None).unwrap();
            assert_eq!(read, PaykitMember::default(), "{m}");
        }
    }

    #[test]
    fn test_app_record_member_refuses_each_rule() {
        for (m, rule) in [
            (
                json!({ "capabilities": [] }),
                "capabilities isn't an object",
            ),
            (
                json!({ "capabilities": { "endpoint_identifiers": "btc" } }),
                "isn't an array",
            ),
            (
                json!({ "capabilities": { "endpoint_identifiers": [1] } }),
                "holds a non-string",
            ),
            (
                json!({ "capabilities": { "endpoint_identifiers": ["btc/lightning"] } }),
                "in capabilities isn't a Payment Endpoint Identifier",
            ),
            (
                json!({ "capabilities": { "endpoint_identifiers": ["private"] } }),
                "in capabilities isn't a Payment Endpoint Identifier",
            ),
            (
                json!({ "public_endpoints": [] }),
                "public_endpoints isn't an object",
            ),
            (
                json!({ "public_endpoints": { "..": "lno1" } }),
                "key isn't a Payment Endpoint Identifier",
            ),
            (
                json!({ "public_endpoints": { "private": "lno1" } }),
                "key isn't a Payment Endpoint Identifier",
            ),
            (
                json!({ "public_endpoints": { "btc-lightning-bolt12": {} } }),
                "payload isn't a string",
            ),
            (
                json!({ "public_endpoints": { "btc-lightning-bolt12": "" } }),
                "payload is empty",
            ),
        ] {
            let error = PaykitMember::from_member(Some(&member(m.clone())), EXPIRES)
                .unwrap_err()
                .to_string();
            assert!(error.contains(rule), "{m}: {error}");
            assert!(!error.contains("lno1"), "{error}");
        }
        let endpoints = member(json!({ "public_endpoints": { "btc-lightning-bolt12": "lno1" } }));
        let error = PaykitMember::from_member(Some(&endpoints), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("carries expires_at"), "{error}");
    }
}
