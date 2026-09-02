use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

type JsonMap = Map<String, Value>;

pub(crate) mod map {
    use super::*;

    pub(crate) fn serialize<S>(value: &JsonMap, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            return value.serialize(serializer);
        }
        serde_json::to_string(value)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<JsonMap, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            return JsonMap::deserialize(deserializer);
        }
        let encoded = String::deserialize(deserializer)?;
        serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
    }
}

pub(crate) mod optional_map {
    use super::*;

    pub(crate) fn serialize<S>(value: &Option<JsonMap>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            return value.serialize(serializer);
        }
        value
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<JsonMap>, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            return Option::<JsonMap>::deserialize(deserializer);
        }
        Option::<String>::deserialize(deserializer)?
            .map(|encoded| serde_json::from_str(&encoded))
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}
