//! Shared publication status values.

use serde::{Deserialize, Serialize};

/// Local publication state for SDK-managed public data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PublicationStatus {
    /// No publication is known to exist.
    #[default]
    NotPublished,
    /// Publication was recorded locally before the remote write.
    #[serde(alias = "Desired")]
    PendingPublication,
    /// Publication is known to exist.
    Published,
    /// Removal was recorded locally before the remote delete.
    PendingRemoval,
    /// Publication is known to be removed.
    Removed,
    /// Last publication or removal attempt failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publication_status_accepts_desired_alias() {
        let status: PublicationStatus = serde_json::from_str("\"Desired\"").unwrap();

        assert_eq!(status, PublicationStatus::PendingPublication);
    }
}
