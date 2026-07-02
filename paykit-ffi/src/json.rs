use std::{fmt, sync::Arc};

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{errors::validation_error, PaykitFfiError};

/// Private JSON object with redacted debug output.
#[derive(uniffi::Object, PartialEq, Eq)]
pub struct FfiPrivateJsonObject {
    text: String,
}

impl fmt::Debug for FfiPrivateJsonObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FfiPrivateJsonObject(<redacted:{} bytes>)",
            self.text.len()
        )
    }
}

impl FfiPrivateJsonObject {
    #[cfg(test)]
    pub(crate) fn from_unchecked_text(text: String) -> Self {
        Self { text }
    }

    pub(crate) fn from_json_map(
        label: &'static str,
        value: &JsonMap<String, JsonValue>,
    ) -> Result<Arc<Self>, PaykitFfiError> {
        json_object_to_string(label, value).map(|text| Arc::new(Self { text }))
    }

    pub(crate) fn parse_map(
        &self,
        label: &'static str,
    ) -> Result<JsonMap<String, JsonValue>, PaykitFfiError> {
        parse_json_object(label, &self.text)
    }
}

#[uniffi::export]
impl FfiPrivateJsonObject {
    /// Create a private JSON object after validating it.
    #[uniffi::constructor]
    pub fn new(text: String) -> Result<Self, PaykitFfiError> {
        parse_json_object("Private JSON object", &text)?;
        Ok(Self { text })
    }

    /// Export the JSON text for explicit app display, storage, or payment execution.
    pub fn export_text(&self) -> String {
        self.text.clone()
    }
}

pub(crate) fn parse_json_object(
    label: &'static str,
    raw: &str,
) -> Result<JsonMap<String, JsonValue>, PaykitFfiError> {
    serde_json::from_str::<JsonMap<String, JsonValue>>(raw)
        .map_err(|err| validation_error(format!("{label} must be a JSON object: {err}")))
}

pub(crate) fn json_object_to_string(
    label: &'static str,
    value: &JsonMap<String, JsonValue>,
) -> Result<String, PaykitFfiError> {
    serde_json::to_string(value).map_err(|err| PaykitFfiError::Protocol {
        code: "serialization".into(),
        context: format!("{label} could not be serialized: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_private_json_object_redacts_debug() {
        let object = FfiPrivateJsonObject::new(r#"{"secret":"value"}"#.into()).unwrap();

        assert_eq!(object.export_text(), r#"{"secret":"value"}"#);
        assert!(!format!("{object:?}").contains("value"));
    }

    #[test]
    fn test_private_json_object_rejects_non_object() {
        assert!(matches!(
            FfiPrivateJsonObject::new("[]".into()),
            Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
        ));
    }
}
