use crate::{PaykitError, Result};

/// UUID-v4 identifier for one Event Message.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EventId(String);

impl EventId {
    /// Create an Event ID from a UUID-v4 string.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let uuid = uuid::Uuid::try_parse(&id).map_err(|err| {
            PaykitError::Validation(format!("Event ID must be a UUID v4 string: {err}"))
        })?;
        if uuid.get_version_num() != 4 || uuid.get_variant() != uuid::Variant::RFC4122 {
            return Err(PaykitError::Validation(
                "Event ID must be an RFC4122 UUID v4 string".into(),
            ));
        }
        Ok(Self(uuid.hyphenated().to_string()))
    }

    /// Generate a fresh Event ID.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Access the canonical UUID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for EventId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
