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

use crate::{EndpointData, MethodId, PaykitError, Result, SupportedPayments};

/// Conventional prefix for public Paykit data hosted on Pubky storage.
///
/// `v0` stores public payment endpoints as:
/// `/pub/paykit/v0/{method_id}` with the file payload being the payment endpoint.
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";

/// Conventional prefix for private (encrypted) Paykit data.
///
/// This prefix is used as the base path for pubky-noise's encrypted messaging.
/// The actual write and read paths are derived per-peer-pair using
/// [`pubky_noise::path_derivation::derive_asymmetric_paths`]. Pubky-noise manages
/// individual file slots within the derived folders using a counter-based scheme.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";

/// Writes or updates a payment endpoint document in the authenticated Pubky session.
#[instrument(skip(session, data), fields(method = %method))]
pub async fn upsert_payment_endpoint(
    session: &PubkySession,
    method: &MethodId,
    data: &EndpointData,
) -> Result<()> {
    let path = payment_endpoint_path(method);
    debug!(path = %path, "writing payment endpoint to Pubky storage");
    session
        .storage()
        .put(path, data.as_str().to_string())
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

/// Removes an existing payment endpoint from the authenticated Pubky session.
#[instrument(skip(session), fields(method = %method))]
pub async fn delete_payment_endpoint(session: &PubkySession, method: &MethodId) -> Result<()> {
    let path = payment_endpoint_path(method);
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

/// Fetches all public payment endpoints for the provided payee from Pubky storage.
///
/// Directory listing and per-entry fetches are not atomic; the returned list is a
/// best-effort snapshot of the payee's homeserver state.
#[instrument(skip(storage), fields(payee = %payee))]
pub async fn fetch_supported_payments(
    storage: &PublicStorage,
    payee: &PublicKey,
) -> Result<SupportedPayments> {
    let addr = format!("{payee}{PAYKIT_PATH_PREFIX}");
    debug!(addr = %addr, "listing supported payment methods");
    let entries = list_entries(storage, addr, "list supported payments").await?;

    let mut map = HashMap::new();
    for resource in entries {
        if resource.path.as_str().ends_with('/') {
            trace!(path = %resource.path, "skipping directory entry");
            continue;
        }

        let method = resource
            .path
            .as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or_else(|| {
                error!(path = %resource.path, "invalid resource path for payment entry");
                PaykitError::InvalidData {
                    context: format!(
                        "cannot extract method from resource path '{}'",
                        resource.path
                    ),
                    source: None,
                }
            })?
            .to_string();

        let label = format!("fetch endpoint {method}");
        if let Some(payload) = fetch_text(storage, resource.to_string(), &label).await? {
            debug!(method = %method, "fetched payment endpoint payload");
            let method_id = MethodId::new(&method).map_err(|err| PaykitError::InvalidData {
                context: format!("storage returned invalid method identifier '{method}'"),
                source: Some(err.into()),
            })?;
            map.insert(method_id, EndpointData::new(payload));
        }
    }

    debug!(count = map.len(), "supported payments collected");
    Ok(SupportedPayments { entries: map })
}

/// Fetches an individual public payment endpoint from Pubky storage.
#[instrument(skip(storage), fields(payee = %payee, method = %method))]
pub async fn fetch_payment_endpoint(
    storage: &PublicStorage,
    payee: &PublicKey,
    method: &MethodId,
) -> Result<Option<EndpointData>> {
    let addr = format!("{payee}{}", payment_endpoint_path(method));
    debug!(addr = %addr, "fetching individual payment endpoint");
    match fetch_text(storage, addr, "fetch endpoint").await? {
        Some(payload) => {
            debug!("payment endpoint found");
            Ok(Some(EndpointData::new(payload)))
        }
        None => {
            debug!("payment endpoint not found");
            Ok(None)
        }
    }
}

fn payment_endpoint_path(method: &MethodId) -> String {
    format!("{PAYKIT_PATH_PREFIX}{}", method.as_str())
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
