//! Private Payment List ordering (Paykit wire 4.13).

use pubky_switch_core::json::MAX_INT;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::{ensure, Error, Result};
use crate::json::{parse_object, Obj};

/// A version reserved for one publisher app and recipient (Paykit spec 8.4.1).
/// Generation precedes counter in both ordering and the writer's representation.
/// Neither member is a time or permission to publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ListVersion {
    generation: u64,
    counter: u64,
}

impl ListVersion {
    /// Construct a pair of positive Switch integers (Paykit wire 4.13).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] if either member is outside 1 through 9007199254740991.
    pub fn new(generation: u64, counter: u64) -> Result<Self> {
        ensure(
            (1..=MAX_INT).contains(&generation) && (1..=MAX_INT).contains(&counter),
            "Paykit wire 4.13: generation and counter must be positive Switch integers",
        )
        .map_err(Error::into_validation)?;
        Ok(Self {
            generation,
            counter,
        })
    }

    /// The publisher identity's verified generation.
    pub fn generation(self) -> u64 {
        self.generation
    }

    /// The recipient's reserved counter in this generation.
    pub fn counter(self) -> u64 {
        self.counter
    }

    /// The next counter in the same generation, or `None` at exhaustion.
    /// This arithmetic supplies no reservation authority (Paykit spec 8.4.1).
    pub fn next(self) -> Option<Self> {
        (self.counter < MAX_INT).then(|| Self {
            generation: self.generation,
            counter: self.counter + 1,
        })
    }

    /// Read the wire object, ignoring unknown members (Paykit wire Conventions, 4.13).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for absent, zero or out-of-range members.
    pub fn from_value(value: &Value) -> Result<Self> {
        let obj = Obj::of(value, "Paykit wire 4.13", "list-version")?;
        Self::new(obj.req_int("generation")?, obj.req_int("counter")?).map_err(
            |error| match error {
                Error::Validation(why) => Error::Invalid(why),
                other => other,
            },
        )
    }

    /// Read complete pair bytes, refusing duplicate members and ignoring unknown wire
    /// members (Paykit wire Conventions, 4.13).
    ///
    /// # Errors
    ///
    /// Malformed JSON or missing, invalid or repeated members.
    pub fn read(json: &[u8]) -> Result<Self> {
        Self::from_value(&Value::Object(parse_object(
            json,
            "Paykit wire 4.13: list-version",
        )?))
    }

    /// Write the fixed compact representation with generation before counter.
    ///
    /// # Errors
    ///
    /// A serialization failure.
    pub fn to_json(self) -> Result<Vec<u8>> {
        serde_json::to_vec(&self)
            .map_err(|_| Error::Invalid("Paykit wire 4.13: cannot write list-version".into()))
    }
}

impl<'de> Deserialize<'de> for ListVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // Protocol-local data is current, not an extensible network object.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Pair {
            generation: u64,
            counter: u64,
        }
        let pair = Pair::deserialize(deserializer)?;
        Self::new(pair.generation, pair.counter).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_list_version_orders_generation_before_counter() {
        let last = ListVersion::new(7, MAX_INT).unwrap();
        let next = ListVersion::new(8, 1).unwrap();
        assert!(next > last);
        assert_eq!(last.next(), None);
        assert_eq!(
            ListVersion::new(7, 11).unwrap().next(),
            Some(ListVersion::new(7, 12).unwrap())
        );
        assert_eq!(
            serde_json::to_string(&next).unwrap(),
            r#"{"generation":8,"counter":1}"#
        );
    }

    #[test]
    fn test_list_version_requires_positive_bounded_members() {
        for value in [
            json!({}),
            json!({"generation":1}),
            json!({"counter":1}),
            json!({"generation":0,"counter":1}),
            json!({"generation":1,"counter":0}),
            json!({"generation":MAX_INT+1,"counter":1}),
            json!({"generation":1,"counter":MAX_INT+1}),
            json!({"generation":1.5,"counter":1}),
            json!({"generation":1,"counter":null}),
        ] {
            assert!(ListVersion::from_value(&value).is_err());
            assert!(serde_json::from_value::<ListVersion>(value).is_err());
        }
        assert!(ListVersion::from_value(&json!({"generation":MAX_INT,"counter":MAX_INT})).is_ok());
    }

    #[test]
    fn test_list_version_current_storage_and_wire_members() {
        let extra = json!({"generation":1,"counter":2,"extra":true});
        assert_eq!(
            ListVersion::from_value(&extra).unwrap(),
            ListVersion::new(1, 2).unwrap()
        );
        assert!(serde_json::from_value::<ListVersion>(extra).is_err());
        for text in [
            r#"{"generation":1,"generation":2,"counter":1}"#,
            r#"{"generation":1,"counter":1,"counter":2}"#,
        ] {
            assert!(serde_json::from_str::<ListVersion>(text).is_err());
        }
    }
}
