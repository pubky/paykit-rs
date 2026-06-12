//! Public Payment Endpoint sync records.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use serde::{Deserialize, Serialize};

use crate::{
    adapters::ReceivingDetail,
    storage::{PublicEndpointRecord, StorageAdapter},
    PaykitSdkError, Result,
};

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

/// One public endpoint changed during sync.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSyncChange {
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Resulting local publication status.
    pub status: EndpointPublicationStatus,
    /// Error text for failed changes.
    pub error: Option<String>,
}

/// Summary returned after public Payment Endpoint sync.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSyncReport {
    /// Endpoints successfully published or updated.
    pub published: Vec<EndpointSyncChange>,
    /// Endpoints successfully removed.
    pub removed: Vec<EndpointSyncChange>,
    /// Endpoints that failed to publish or remove.
    pub failed: Vec<EndpointSyncChange>,
}

pub(crate) fn normalize_receiving_details(
    details: Vec<ReceivingDetail>,
) -> Result<HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
    let mut desired = HashMap::with_capacity(details.len());

    for detail in details {
        let identifier = PaymentEndpointIdentifier::new(detail.identifier)?;
        if desired.contains_key(&identifier) {
            return Err(PaykitSdkError::Protocol(format!(
                "duplicate Payment Endpoint identifier '{}'",
                identifier.as_str()
            )));
        }

        desired.insert(identifier, PaymentEndpointPayload::new(detail.payload));
    }

    Ok(desired)
}

/// Load SDK-managed public endpoint records.
pub async fn load_public_endpoint_records<S>(storage: &S) -> Result<Vec<PublicEndpointRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.public_endpoint_records()))
        .await
}

pub(crate) fn published_record(
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
    now: DateTime<Utc>,
) -> PublicEndpointRecord {
    PublicEndpointRecord {
        identifier: identifier.as_str().to_owned(),
        payload: Some(payload.as_str().to_owned()),
        status: EndpointPublicationStatus::Published,
        updated_at: now,
        last_error: None,
    }
}

pub(crate) fn desired_record(
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
    now: DateTime<Utc>,
) -> PublicEndpointRecord {
    PublicEndpointRecord {
        identifier: identifier.as_str().to_owned(),
        payload: Some(payload.as_str().to_owned()),
        status: EndpointPublicationStatus::Desired,
        updated_at: now,
        last_error: None,
    }
}

pub(crate) fn pending_removal_record(
    identifier: String,
    payload: Option<String>,
    now: DateTime<Utc>,
) -> PublicEndpointRecord {
    PublicEndpointRecord {
        identifier,
        payload,
        status: EndpointPublicationStatus::PendingRemoval,
        updated_at: now,
        last_error: None,
    }
}

pub(crate) fn removed_record(identifier: String, now: DateTime<Utc>) -> PublicEndpointRecord {
    PublicEndpointRecord {
        identifier,
        payload: None,
        status: EndpointPublicationStatus::Removed,
        updated_at: now,
        last_error: None,
    }
}

pub(crate) fn failed_record(
    identifier: String,
    payload: Option<String>,
    error: String,
    now: DateTime<Utc>,
) -> PublicEndpointRecord {
    PublicEndpointRecord {
        identifier,
        payload,
        status: EndpointPublicationStatus::Failed,
        updated_at: now,
        last_error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_receiving_details_rejects_invalid_identifier() {
        let result = normalize_receiving_details(vec![ReceivingDetail {
            identifier: "../bad".into(),
            payload: "payload".into(),
        }]);

        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_receiving_details_rejects_duplicates() {
        let result = normalize_receiving_details(vec![
            ReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "one".into(),
            },
            ReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "two".into(),
            },
        ]);

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }
}
