//! Telemetry and redaction settings.

use serde::{Deserialize, Serialize};

/// Log redaction level for SDK telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedactionLevel {
    /// Redact secrets and sensitive private payloads.
    Standard,
    /// Redact all private payload details.
    Strict,
}
