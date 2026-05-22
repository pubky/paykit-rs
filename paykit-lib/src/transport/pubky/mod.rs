//! Pubky storage helpers and shared constants.

use std::collections::HashMap;

use pubky::{
    errors::RequestError, Error as PubkyError, PubkyResource, PubkySession, PublicStorage,
    StatusCode,
};
use tracing::{debug, error, instrument, trace};

use crate::{
    PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList, PublicKey, Result,
};

/// Conventional prefix for public Paykit data hosted on Pubky storage.
/// `v0` means that the paykit conventions is to store data on pubky as following:
///  - /pub/paykit/v0/{payment_endpoint_identifier} -> with payload being the Payment Endpoint Payload
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";
/// Conventional prefix for private (encrypted) Paykit data.
/// This prefix is used as the base path for pubky-noise's encrypted messaging.
/// The actual write and read paths are derived per-peer-pair using
/// [`pubky_noise::path_derivation::derive_asymmetric_paths`]. Pubky-noise manages
/// individual file slots within the derived folders using a counter-based scheme.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";

/// Writes or updates a public Payment Endpoint in the caller's Pubky storage.
#[instrument(skip(session, payload), fields(identifier = %identifier))]
pub async fn upsert_payment_endpoint(
    session: &PubkySession,
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
) -> Result<()> {
    let path = format!("{PAYKIT_PATH_PREFIX}{}", identifier.as_str());
    debug!(path = %path, "writing payment endpoint to Pubky storage");
    session
        .storage()
        .put(path, payload.as_str().to_string())
        .await
        .map_err(|err| {
            error!(error = %err, "failed to put payment endpoint");
            PaykitError::Transport {
                context: "put endpoint".into(),
                source: err.into(),
            }
        })?;
    debug!("payment endpoint stored successfully");
    Ok(())
}

/// Removes a public Payment Endpoint from the caller's Pubky storage.
#[instrument(skip(session), fields(identifier = %identifier))]
pub async fn remove_payment_endpoint(
    session: &PubkySession,
    identifier: &PaymentEndpointIdentifier,
) -> Result<()> {
    let path = format!("{PAYKIT_PATH_PREFIX}{}", identifier.as_str());
    debug!(path = %path, "deleting payment endpoint from Pubky storage");
    session.storage().delete(path).await.map_err(|err| {
        error!(error = %err, "failed to delete payment endpoint");
        PaykitError::Transport {
            context: "delete endpoint".into(),
            source: err.into(),
        }
    })?;
    debug!("payment endpoint removed successfully");
    Ok(())
}

/// Fetches the payee's public Payment List from Pubky storage.
///
/// This first lists Payment Endpoint entries and then fetches each one
/// individually. Pubky storage does not provide an atomic directory snapshot for
/// this operation, so the returned [`PaymentList`] is best-effort if the payee
/// mutates entries concurrently.
#[instrument(skip(storage), fields(payee = %payee))]
pub async fn fetch_payment_list(storage: &PublicStorage, payee: &PublicKey) -> Result<PaymentList> {
    let addr = format!("{payee}{PAYKIT_PATH_PREFIX}");
    debug!(addr = %addr, "listing Payment Endpoint entries");
    let entries = list_entries(storage, addr, "list payment endpoints").await?;

    let mut map = HashMap::new();
    for resource in entries {
        if resource.path.as_str().ends_with('/') {
            trace!(path = %resource.path, "skipping directory entry");
            continue;
        }

        let identifier = resource
            .path
            .as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or_else(|| {
                error!(path = %resource.path, "invalid resource path for payment entry");
                PaykitError::InvalidData {
                    context: format!(
                        "cannot extract Payment Endpoint Identifier from resource path '{}'",
                        resource.path
                    ),
                    source: None,
                }
            })?
            .to_string();

        let label = format!("fetch endpoint {identifier}");
        if let Some(payload) = fetch_text(storage, resource.to_string(), &label).await? {
            debug!(identifier = %identifier, "fetched payment endpoint payload");
            let payment_endpoint_identifier =
                PaymentEndpointIdentifier::new(&identifier).map_err(|err| {
                    PaykitError::InvalidData {
                        context: format!(
                            "storage returned invalid Payment Endpoint Identifier '{}'",
                            identifier
                        ),
                        source: Some(err.into()),
                    }
                })?;
            map.insert(
                payment_endpoint_identifier,
                PaymentEndpointPayload::new(payload),
            );
        }
    }

    debug!(count = map.len(), "payment list collected");
    Ok(PaymentList { endpoints: map })
}

/// Fetches an individual public Payment Endpoint document if it exists.
#[instrument(skip(storage), fields(payee = %payee, identifier = %identifier))]
pub async fn fetch_payment_endpoint(
    storage: &PublicStorage,
    payee: &PublicKey,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    let addr = format!("{payee}{PAYKIT_PATH_PREFIX}{}", identifier.as_str());
    debug!(addr = %addr, "fetching individual payment endpoint");
    match fetch_text(storage, addr, "fetch endpoint").await? {
        Some(payload) => {
            debug!("payment endpoint found");
            Ok(Some(PaymentEndpointPayload::new(payload)))
        }
        None => {
            debug!("payment endpoint not found");
            Ok(None)
        }
    }
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
async fn list_entries(
    storage: &PublicStorage,
    addr: String,
    label: &str,
) -> Result<Vec<PubkyResource>> {
    trace!("listing directory entries");
    let builder = match storage.list(&addr) {
        Ok(builder) => builder,
        Err(err) if is_not_found(&err) => {
            debug!("directory not found, returning empty list");
            return Ok(Vec::new());
        }
        Err(err) => {
            error!(error = %err, "failed to create list builder");
            return Err(PaykitError::Transport {
                context: label.to_string(),
                source: err.into(),
            });
        }
    };

    match builder.shallow(true).send().await {
        Ok(entries) => {
            debug!(count = entries.len(), "directory entries listed");
            Ok(entries)
        }
        Err(err) if is_not_found(&err) => {
            debug!("directory not found during send, returning empty list");
            Ok(Vec::new())
        }
        Err(err) => {
            error!(error = %err, "list send failed");
            Err(PaykitError::Transport {
                context: format!("{label} send failed"),
                source: err.into(),
            })
        }
    }
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}
