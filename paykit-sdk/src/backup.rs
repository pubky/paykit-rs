//! SDK-managed backup records.

use serde::{Deserialize, Serialize};

/// Metadata for an SDK backup payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkBackupMetadata {
    /// SDK backup schema version.
    pub version: u32,
}
