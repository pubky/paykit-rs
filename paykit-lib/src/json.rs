//! Reading the members of Paykit's JSON objects (Switch wire 1.1, Paykit wire Conventions).
//!
//! Readers ignore unknown members and read `null` as a missing member, unless a table gives
//! `null` its own meaning. Error text names the section and the member, never the value.

use pubky_switch_core::enc;
use pubky_switch_core::time::parse_rfc3339;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// The members of one JSON object, read under the section `ctx` names.
#[derive(Clone, Copy)]
pub(crate) struct Obj<'a> {
    map: &'a Map<String, Value>,
    ctx: &'a str,
}

/// A failure naming the section and the member.
pub(crate) fn bad(ctx: &str, member: &str, rule: &str) -> Error {
    Error::Invalid(format!("{ctx}: {member}: {rule}"))
}

impl<'a> Obj<'a> {
    /// An object's members.
    pub(crate) fn new(map: &'a Map<String, Value>, ctx: &'a str) -> Self {
        Self { map, ctx }
    }

    /// A value that must be an object.
    pub(crate) fn of(value: &'a Value, ctx: &'a str, what: &str) -> Result<Self> {
        value
            .as_object()
            .map(|map| Self { map, ctx })
            .ok_or_else(|| bad(ctx, what, "not an object"))
    }

    /// The section this object is read under.
    pub(crate) fn ctx(&self) -> &'a str {
        self.ctx
    }

    /// A member, `None` when missing or `null`.
    pub(crate) fn opt(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    /// Whether the member is present, `null` included: for members a writer always writes.
    pub(crate) fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// A required member.
    pub(crate) fn req(&self, key: &str) -> Result<&'a Value> {
        self.opt(key).ok_or_else(|| bad(self.ctx, key, "missing"))
    }

    /// A required object member.
    pub(crate) fn req_obj(&self, key: &str) -> Result<Obj<'a>> {
        Obj::of(self.req(key)?, self.ctx, key)
    }

    /// An optional object member.
    pub(crate) fn opt_obj(&self, key: &str) -> Result<Option<Obj<'a>>> {
        self.opt(key).map(|v| Obj::of(v, self.ctx, key)).transpose()
    }

    /// A required string member.
    pub(crate) fn req_str(&self, key: &str) -> Result<&'a str> {
        self.req(key)?
            .as_str()
            .ok_or_else(|| bad(self.ctx, key, "not a string"))
    }

    /// An optional string member.
    pub(crate) fn opt_str(&self, key: &str) -> Result<Option<&'a str>> {
        self.opt(key)
            .map(|v| v.as_str().ok_or_else(|| bad(self.ctx, key, "not a string")))
            .transpose()
    }

    /// A required integer member (Switch wire 1.1: 0 to 2^53 - 1).
    pub(crate) fn req_int(&self, key: &str) -> Result<u64> {
        as_int(self.req(key)?, self.ctx, key)
    }

    /// An optional integer member.
    pub(crate) fn opt_int(&self, key: &str) -> Result<Option<u64>> {
        self.opt(key).map(|v| as_int(v, self.ctx, key)).transpose()
    }

    /// An optional boolean member.
    pub(crate) fn opt_bool(&self, key: &str) -> Result<Option<bool>> {
        self.opt(key)
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| bad(self.ctx, key, "not a boolean"))
            })
            .transpose()
    }

    /// A required array member.
    pub(crate) fn req_arr(&self, key: &str) -> Result<&'a Vec<Value>> {
        self.req(key)?
            .as_array()
            .ok_or_else(|| bad(self.ctx, key, "not an array"))
    }

    /// An optional array member.
    pub(crate) fn opt_arr(&self, key: &str) -> Result<Option<&'a Vec<Value>>> {
        self.opt(key)
            .map(|v| {
                v.as_array()
                    .ok_or_else(|| bad(self.ctx, key, "not an array"))
            })
            .transpose()
    }

    /// A required time member, in Unix milliseconds (Switch wire 1.6).
    pub(crate) fn req_time(&self, key: &str) -> Result<u64> {
        time(self.req_str(key)?, self.ctx, key)
    }

    /// An optional time member.
    pub(crate) fn opt_time(&self, key: &str) -> Result<Option<u64>> {
        self.opt_str(key)?
            .map(|t| time(t, self.ctx, key))
            .transpose()
    }

    /// A required base64url member of exactly `N` bytes (Switch wire 1.2).
    pub(crate) fn req_b64<const N: usize>(&self, key: &str) -> Result<[u8; N]> {
        b64::<N>(self.req_str(key)?, self.ctx, key)
    }

    /// An optional base64url member of exactly `N` bytes.
    pub(crate) fn opt_b64<const N: usize>(&self, key: &str) -> Result<Option<[u8; N]>> {
        self.opt_str(key)?
            .map(|t| b64::<N>(t, self.ctx, key))
            .transpose()
    }

    /// A required Pubky ID member, as its key (Switch wire 1.3).
    pub(crate) fn req_pubky(&self, key: &str) -> Result<[u8; 32]> {
        enc::pubky_key(self.req_str(key)?).map_err(|_| bad(self.ctx, key, "not a Pubky ID"))
    }

    /// A required App ID member, as its 16 bytes (Switch wire 1.3).
    pub(crate) fn req_app_id(&self, key: &str) -> Result<[u8; 16]> {
        app_id(self.req_str(key)?, self.ctx, key)
    }

    /// An optional App ID member.
    pub(crate) fn opt_app_id(&self, key: &str) -> Result<Option<[u8; 16]>> {
        self.opt_str(key)?
            .map(|t| app_id(t, self.ctx, key))
            .transpose()
    }
}

/// An integer of Switch wire 1.1.
pub(crate) fn as_int(value: &Value, ctx: &str, key: &str) -> Result<u64> {
    value
        .as_u64()
        .filter(|n| *n <= pubky_switch_core::json::MAX_INT)
        .ok_or_else(|| bad(ctx, key, "not an integer of Switch wire 1.1"))
}

/// A time of Switch wire 1.6, in Unix milliseconds.
pub(crate) fn time(text: &str, ctx: &str, key: &str) -> Result<u64> {
    parse_rfc3339(text).map_err(|_| bad(ctx, key, "not a time of Switch wire 1.6"))
}

/// Base64url of exactly `N` bytes (Switch wire 1.2).
pub(crate) fn b64<const N: usize>(text: &str, ctx: &str, key: &str) -> Result<[u8; N]> {
    enc::unb64_array::<N>(text).map_err(|_| bad(ctx, key, &format!("not base64url of {N} bytes")))
}

/// An App ID's 16 bytes (Switch wire 1.3).
pub(crate) fn app_id(text: &str, ctx: &str, key: &str) -> Result<[u8; 16]> {
    enc::app_id_decode(text).map_err(|_| bad(ctx, key, "not an App ID"))
}

/// Parse a JSON document strictly (Switch wire 1.1) into an object.
pub(crate) fn parse_object(bytes: &[u8], ctx: &str) -> Result<Map<String, Value>> {
    let value: Value = pubky_switch_core::json::parse_json(bytes)?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Error::Invalid(format!("{ctx}: not a JSON object"))),
    }
}

/// Check the version member `v` of a shared record value: 1 (Paykit wire 5).
pub(crate) fn check_v1(obj: &Obj<'_>) -> Result<()> {
    if obj.req_int("v")? != 1 {
        return Err(bad(obj.ctx(), "v", "isn't 1"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(value: &Value) -> Obj<'_> {
        Obj::of(value, "ctx", "test").unwrap()
    }

    #[test]
    fn test_json_null_reads_as_missing() {
        let value = json!({ "a": null, "b": "x" });
        let o = obj(&value);
        assert!(o.opt("a").is_none());
        assert!(o.has("a"));
        assert!(o.req("a").is_err());
        assert_eq!(o.req_str("b").unwrap(), "x");
    }

    #[test]
    fn test_json_errors_name_the_member_not_the_value() {
        let value = json!({ "n": "secret-text", "t": "secret-time" });
        let o = obj(&value);
        let error = o.req_int("n").unwrap_err().to_string();
        assert!(error.contains("n:") && !error.contains("secret"), "{error}");
        let error = o.req_time("t").unwrap_err().to_string();
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn test_json_integers_follow_the_switch_bound() {
        let value = json!({ "ok": 9_007_199_254_740_991u64, "big": 9_007_199_254_740_992u64 });
        let o = obj(&value);
        assert!(o.req_int("ok").is_ok());
        assert!(o.req_int("big").is_err());
    }

    #[test]
    fn test_json_parse_object_refuses_other_values() {
        assert!(parse_object(b"{\"v\":1}", "ctx").is_ok());
        assert!(parse_object(b"[1]", "ctx").is_err());
        assert!(parse_object(b"{\"v\":1.5}", "ctx").is_err());
    }
}
