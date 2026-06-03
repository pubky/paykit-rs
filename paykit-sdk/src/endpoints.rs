//! Public Payment Endpoint sync records.

use serde::{Deserialize, Serialize};

/// Publication status for a SDK-managed Payment Endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointPublicationStatus {
    /// The endpoint should be published.
    Desired,
    /// The endpoint is confirmed as published.
    Published,
    /// The endpoint should be removed.
    PendingRemoval,
    /// The endpoint is confirmed removed.
    Removed,
    /// The last publication attempt failed.
    Failed,
}
