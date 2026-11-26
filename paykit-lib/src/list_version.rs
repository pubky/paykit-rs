//! Private Payment List ordering (Paykit wire 4.13).

use pubky_dispatch_core::json::MAX_INT;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::{ensure, Error, Result};
use crate::json::Obj;

/// A Private Payment List version (Paykit wire 4.13): the publishing identity's generation
/// and a counter of the publishing app for one recipient. Versions compare generation
/// first, then counter. Neither member is a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListVersion {
    generation: u64,
    counter: u64,
}

impl ListVersion {
    /// Construct a pair of positive Dispatch integers (Paykit wire 4.13).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] if either member is outside 1 through 9007199254740991.
    pub fn new(generation: u64, counter: u64) -> Result<Self> {
        ensure(
            (1..=MAX_INT).contains(&generation) && (1..=MAX_INT).contains(&counter),
            "Paykit wire 4.13: generation and counter must be positive Dispatch integers",
        )
        .map_err(Error::into_validation)?;
        Ok(Self {
            generation,
            counter,
        })
    }

    /// The generation the pair carries. It establishes no identity document.
    pub fn generation(self) -> u64 {
        self.generation
    }

    /// The counter the pair carries. It establishes no reservation.
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

    /// The wire object (Paykit wire 4.13). Members may be written in any order (Dispatch
    /// wire 1.1); this writes them sorted by name, as every Paykit body.
    pub fn to_value(self) -> Value {
        let mut map = Map::new();
        map.insert("counter".into(), Value::from(self.counter));
        map.insert("generation".into(), Value::from(self.generation));
        Value::Object(map)
    }
}

impl Serialize for ListVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ListVersion {
    /// Refuse a repeated member name (Dispatch wire 1.1), then the one reader,
    /// [`ListVersion::from_value`], for every copy of a pair.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Members;

        impl<'de> Visitor<'de> for Members {
            type Value = Map<String, Value>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a list-version object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut members = Map::new();
                while let Some((name, value)) = access.next_entry::<String, Value>()? {
                    if members.contains_key(&name) {
                        return Err(serde::de::Error::custom(format!(
                            "Paykit wire 4.13: repeated list-version member {name}"
                        )));
                    }
                    members.insert(name, value);
                }
                Ok(members)
            }
        }

        let members = deserializer.deserialize_map(Members)?;
        Self::from_value(&Value::Object(members)).map_err(serde::de::Error::custom)
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
        assert!(ListVersion::new(7, 12).unwrap() > ListVersion::new(7, 11).unwrap());
        assert_eq!(last.next(), None);
        assert_eq!(
            ListVersion::new(7, 11).unwrap().next(),
            Some(ListVersion::new(7, 12).unwrap())
        );
    }

    #[test]
    fn test_list_version_requires_positive_bounded_members() {
        for value in [
            json!({}),
            json!(7),
            json!({"generation":1}),
            json!({"counter":1}),
            json!({"generation":0,"counter":1}),
            json!({"generation":1,"counter":0}),
            json!({"generation":MAX_INT+1,"counter":1}),
            json!({"generation":1,"counter":MAX_INT+1}),
            json!({"generation":1.5,"counter":1}),
            json!({"generation":1,"counter":null}),
        ] {
            assert!(ListVersion::from_value(&value).is_err(), "{value}");
            assert!(serde_json::from_value::<ListVersion>(value).is_err());
        }
        assert!(ListVersion::from_value(&json!({"generation":MAX_INT,"counter":MAX_INT})).is_ok());
    }

    #[test]
    fn test_list_version_one_reader_for_every_copy() {
        // Unknown members are ignored and members may come in any order (Dispatch wire 1.1),
        // by the wire reader and by serde alike.
        for text in [
            r#"{"generation":1,"counter":2,"extra":true}"#,
            r#"{"counter":2,"generation":1}"#,
        ] {
            let value: Value = serde_json::from_str(text).unwrap();
            let pair = ListVersion::new(1, 2).unwrap();
            assert_eq!(ListVersion::from_value(&value).unwrap(), pair);
            assert_eq!(serde_json::from_str::<ListVersion>(text).unwrap(), pair);
        }
        let pair = ListVersion::new(8, 1).unwrap();
        assert_eq!(
            serde_json::to_value(pair).unwrap(),
            json!({"generation":8,"counter":1})
        );
        assert_eq!(serde_json::to_value(pair).unwrap(), pair.to_value());
    }

    #[test]
    fn test_list_version_serde_refuses_repeated_members() {
        for text in [
            r#"{"generation":1,"generation":2,"counter":1}"#,
            r#"{"generation":1,"counter":1,"counter":2}"#,
        ] {
            assert!(serde_json::from_str::<ListVersion>(text).is_err(), "{text}");
        }
        // A repeated unknown member is refused as well; distinct unknown members are kept
        // out of the pair and ignored.
        assert!(
            serde_json::from_str::<ListVersion>(r#"{"generation":1,"counter":1,"x":1,"x":2}"#)
                .is_err()
        );
        assert_eq!(
            serde_json::from_str::<ListVersion>(r#"{"generation":1,"counter":1,"x":1,"y":2}"#)
                .unwrap(),
            ListVersion::new(1, 1).unwrap()
        );
    }

    #[test]
    fn test_list_version_new_bounds() {
        assert!(ListVersion::new(MAX_INT, MAX_INT).is_ok());
        assert!(ListVersion::new(1, 1).is_ok());
        for (generation, counter) in [(0, 1), (1, 0), (MAX_INT + 1, 1), (1, MAX_INT + 1)] {
            assert!(
                matches!(
                    ListVersion::new(generation, counter),
                    Err(Error::Validation(_))
                ),
                "({generation}, {counter})"
            );
        }
    }
}
