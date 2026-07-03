//! Concrete Pubky routing/storage helpers used by Paykit.
//!
//! Paykit supports Pubky as its storage and encrypted-message transport. This
//! module centralizes public payment endpoint path construction and public
//! storage access so call sites do not hard-code Pubky paths.

use std::collections::HashMap;

use pubky::{
    errors::RequestError, Error as PubkyError, PubkyResource, PubkySession, PublicKey,
    PublicStorage, StatusCode,
};
use tracing::{debug, error, instrument, trace};

use crate::{
    PaykitError, PaykitReceiverId, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList,
    Result,
};

/// Conventional prefix for receiver-scoped Paykit public data.
pub const PAYKIT_RECEIVERS_PATH_PREFIX: &str = "/pub/paykit/v0/receivers";

/// Conventional prefix for receiver-scoped Paykit private data.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";

const LIST_PAGE_LIMIT: u16 = 100;

/// Writes or updates a receiver-scoped Payment Endpoint document.
#[instrument(skip(session, payload), fields(receiver = %receiver_id, identifier = %identifier))]
pub async fn upsert_payment_endpoint(
    session: &PubkySession,
    receiver_id: &PaykitReceiverId,
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
) -> Result<()> {
    let path = payment_endpoint_path(receiver_id, identifier);
    debug!(path = %path, "writing Payment Endpoint to Pubky storage");
    session
        .storage()
        .put(path, payload.as_str().to_string())
        .await
        .map_err(|err| {
            error!(error = %err, "failed to put Payment Endpoint");
            PaykitError::Transport {
                context: "put Payment Endpoint".into(),
                source: err.into(),
            }
        })?;
    Ok(())
}

/// Removes a receiver-scoped Payment Endpoint from the authenticated Pubky session.
#[instrument(skip(session), fields(receiver = %receiver_id, identifier = %identifier))]
pub async fn delete_payment_endpoint(
    session: &PubkySession,
    receiver_id: &PaykitReceiverId,
    identifier: &PaymentEndpointIdentifier,
) -> Result<()> {
    let path = payment_endpoint_path(receiver_id, identifier);
    debug!(path = %path, "deleting Payment Endpoint from Pubky storage");
    match session.storage().delete(path).await {
        Ok(_) => {}
        Err(err) if is_not_found(&err) => {
            debug!("Payment Endpoint already absent");
        }
        Err(err) => {
            error!(error = %err, "failed to delete Payment Endpoint");
            return Err(PaykitError::Transport {
                context: "delete Payment Endpoint".into(),
                source: err.into(),
            });
        }
    }
    Ok(())
}

/// Fetches all public Payment Endpoints for one receiver from Pubky storage.
///
/// Directory listing and per-resource fetches are not atomic; the returned list is a
/// best-effort snapshot of the payee's homeserver state.
#[instrument(skip(storage), fields(payee = %payee, receiver = %receiver_id))]
pub async fn fetch_payment_list(
    storage: &PublicStorage,
    payee: &PublicKey,
    receiver_id: &PaykitReceiverId,
) -> Result<PaymentList> {
    let addr = format!("{payee}{}", payment_endpoint_path_prefix(receiver_id));
    debug!(addr = %addr, "listing Payment Endpoints");
    fetch_payment_list_from_directory(storage, addr).await
}

/// Lists public Paykit receiver ids published by an identity.
#[instrument(skip(storage), fields(owner = %owner))]
pub async fn fetch_paykit_receiver_ids(
    storage: &PublicStorage,
    owner: &PublicKey,
) -> Result<Vec<PaykitReceiverId>> {
    let addr = format!("{owner}{}", receiver_path_prefix());
    debug!(addr = %addr, "listing Paykit receivers");
    let resources = list_resources(storage, addr, "list Paykit receivers").await?;
    let mut receiver_ids = Vec::new();

    for resource in resources {
        let path = resource.path.as_str();
        let prefix = receiver_path_prefix();
        let suffix = path
            .strip_prefix(&prefix)
            .ok_or_else(|| PaykitError::InvalidData {
                context: format!("Paykit receiver path has unexpected prefix: '{path}'"),
                source: None,
            })?;
        let segment = suffix
            .split('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or_else(|| PaykitError::InvalidData {
                context: format!("cannot extract Paykit receiver id from path '{path}'"),
                source: None,
            })?;
        let receiver_id =
            PaykitReceiverId::new(segment).map_err(|err| PaykitError::InvalidData {
                context: format!("storage returned invalid Paykit receiver id '{segment}'"),
                source: Some(err.into()),
            })?;
        if !receiver_ids.contains(&receiver_id) {
            receiver_ids.push(receiver_id);
        }
    }

    Ok(receiver_ids)
}

/// Fetches an individual receiver-scoped public Payment Endpoint.
#[instrument(skip(storage), fields(payee = %payee, receiver = %receiver_id, identifier = %identifier))]
pub async fn fetch_payment_endpoint(
    storage: &PublicStorage,
    payee: &PublicKey,
    receiver_id: &PaykitReceiverId,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    let addr = format!("{payee}{}", payment_endpoint_path(receiver_id, identifier));
    debug!(addr = %addr, "fetching Payment Endpoint");
    match fetch_text(storage, addr, "fetch Payment Endpoint").await? {
        Some(payload) => Ok(Some(PaymentEndpointPayload::new(payload))),
        None => Ok(None),
    }
}

/// Return the receiver registry path prefix.
pub(crate) fn receiver_path_prefix() -> String {
    format!("{PAYKIT_RECEIVERS_PATH_PREFIX}/")
}

/// Return the receiver-scoped public Payment Endpoint path prefix.
pub(crate) fn payment_endpoint_path_prefix(receiver_id: &PaykitReceiverId) -> String {
    format!("{PAYKIT_RECEIVERS_PATH_PREFIX}/{receiver_id}/endpoints/")
}

/// Return the receiver-scoped public Payment Endpoint path.
pub(crate) fn payment_endpoint_path(
    receiver_id: &PaykitReceiverId,
    identifier: &PaymentEndpointIdentifier,
) -> String {
    format!(
        "{}{}",
        payment_endpoint_path_prefix(receiver_id),
        identifier.as_str()
    )
}

/// Return the receiver-scoped private message base path.
pub(crate) fn private_message_path_prefix(receiver_id: &PaykitReceiverId) -> String {
    format!("{PAYKIT_PRIVATE_PATH_PREFIX}/{receiver_id}/messages")
}

/// Return the receiver-scoped Receipt Location prefix.
pub(crate) fn receipt_path_prefix(receiver_id: &PaykitReceiverId) -> String {
    format!("{PAYKIT_PRIVATE_PATH_PREFIX}/{receiver_id}/receipts")
}

/// Return the receiver-scoped Encrypted Link recovery marker base path.
pub(crate) fn encrypted_link_recovery_path_prefix(receiver_id: &PaykitReceiverId) -> String {
    format!("{PAYKIT_PRIVATE_PATH_PREFIX}/{receiver_id}/encrypted-link-recovery")
}

async fn fetch_payment_list_from_directory(
    storage: &PublicStorage,
    addr: String,
) -> Result<PaymentList> {
    let resources = list_resources(storage, addr, "list payment endpoints").await?;

    let mut map = HashMap::new();
    for resource in resources {
        if resource.path.as_str().ends_with('/') {
            trace!(path = %resource.path, "skipping directory resource");
            continue;
        }

        let identifier_text = resource
            .path
            .as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or_else(|| {
                error!(path = %resource.path, "invalid resource path for Payment Endpoint");
                PaykitError::InvalidData {
                    context: format!(
                        "cannot extract Payment Endpoint Identifier from resource path '{}'",
                        resource.path
                    ),
                    source: None,
                }
            })?
            .to_string();

        let label = format!("fetch payment endpoint {identifier_text}");
        if let Some(payload) = fetch_text(storage, resource.to_string(), &label).await? {
            let payment_endpoint_identifier = PaymentEndpointIdentifier::new(&identifier_text)
                .map_err(|err| PaykitError::InvalidData {
                    context: format!(
                        "storage returned invalid Payment Endpoint Identifier '{identifier_text}'"
                    ),
                    source: Some(err.into()),
                })?;
            map.insert(
                payment_endpoint_identifier,
                PaymentEndpointPayload::new(payload),
            );
        }
    }

    Ok(PaymentList {
        payment_endpoints: map,
    })
}

#[instrument(skip(storage), fields(addr = %addr, label = %label))]
async fn fetch_text(storage: &PublicStorage, addr: String, label: &str) -> Result<Option<String>> {
    trace!("fetching text resource");
    match storage.get(&addr).await {
        Ok(resp) => {
            let bytes = resp.bytes().await.map_err(|err| {
                error!(error = %err, "failed to read response bytes");
                PaykitError::Transport {
                    context: label.to_string(),
                    source: err.into(),
                }
            })?;
            if bytes.is_empty() {
                debug!("resource is empty, returning None");
                return Ok(None);
            }
            let data = String::from_utf8(bytes.to_vec()).map_err(|err| {
                let pos = err.utf8_error().valid_up_to();
                error!(
                    error = %err,
                    valid_up_to = pos,
                    "response contains invalid UTF-8 — data may be corrupt"
                );
                PaykitError::InvalidData {
                    context: format!("{label}: invalid UTF-8 at byte {pos}"),
                    source: Some(err.into()),
                }
            })?;
            trace!(len = data.len(), "text resource fetched");
            Ok(Some(data))
        }
        Err(err) if is_not_found(&err) => {
            debug!("resource not found (404/GONE)");
            Ok(None)
        }
        Err(err) => {
            error!(error = %err, "transport error during fetch");
            Err(PaykitError::Transport {
                context: label.to_string(),
                source: err.into(),
            })
        }
    }
}

#[instrument(skip(storage), fields(addr = %addr, label = %label))]
async fn list_resources(
    storage: &PublicStorage,
    addr: String,
    label: &str,
) -> Result<Vec<PubkyResource>> {
    trace!("listing directory resources");
    let mut resources = Vec::new();
    let mut cursor = None::<String>;

    loop {
        let mut builder = match storage.list(&addr) {
            Ok(builder) => builder.shallow(true).limit(LIST_PAGE_LIMIT),
            Err(err) if is_not_found(&err) => {
                debug!("directory not found, returning listed resources");
                return Ok(resources);
            }
            Err(err) => {
                error!(error = %err, "failed to create list builder");
                return Err(PaykitError::Transport {
                    context: label.to_string(),
                    source: err.into(),
                });
            }
        };

        if let Some(cursor) = cursor.as_deref() {
            builder = builder.cursor(cursor);
        }

        let page = match builder.send().await {
            Ok(page) => page,
            Err(err) if is_not_found(&err) => {
                debug!("directory not found during send, returning listed resources");
                return Ok(resources);
            }
            Err(err) => {
                error!(error = %err, "list send failed");
                return Err(PaykitError::Transport {
                    context: format!("{label} send failed"),
                    source: err.into(),
                });
            }
        };

        if page.is_empty() {
            break;
        }

        let page_len = page.len();
        cursor = page
            .last()
            .map(|resource| format!("{}{}", resource.owner.z32(), resource.path.as_str()));
        resources.extend(page);

        if page_len < LIST_PAGE_LIMIT as usize {
            break;
        }
    }

    debug!(count = resources.len(), "directory resources listed");
    Ok(resources)
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receiver_scoped_payment_endpoint_path() {
        let receiver_id = PaykitReceiverId::new("bitkit-9f3a").unwrap();
        let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();

        assert_eq!(
            payment_endpoint_path(&receiver_id, &identifier),
            "/pub/paykit/v0/receivers/bitkit-9f3a/endpoints/btc-lightning-bolt11"
        );
        assert_eq!(receiver_path_prefix(), "/pub/paykit/v0/receivers/");
        assert_eq!(
            private_message_path_prefix(&receiver_id),
            "/pub/paykit/v0/private/bitkit-9f3a/messages"
        );
        assert_eq!(
            receipt_path_prefix(&receiver_id),
            "/pub/paykit/v0/private/bitkit-9f3a/receipts"
        );
        assert_eq!(
            encrypted_link_recovery_path_prefix(&receiver_id),
            "/pub/paykit/v0/private/bitkit-9f3a/encrypted-link-recovery"
        );
    }
}
