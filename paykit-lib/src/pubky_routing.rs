//! Concrete Pubky routing/storage helpers used by Paykit.
//!
//! Paykit supports Pubky as its storage and encrypted-message transport. This
//! module centralizes public payment endpoint path construction and public
//! storage access so call sites do not hard-code Pubky paths.

use std::collections::{HashMap, HashSet};

use pubky::{
    errors::RequestError, Error as PubkyError, PubkyResource, PubkySession, PublicKey,
    PublicStorage, StatusCode,
};
use tracing::{debug, error, instrument, trace};

use crate::{
    PaykitError, PaykitReceiverPath, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentList, Result,
};

/// Conventional prefix for Paykit public data.
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0";

/// Conventional prefix for receiver-scoped Paykit private data.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";

const LIST_PAGE_LIMIT: u16 = 100;

/// Writes or updates a receiver-scoped Payment Endpoint document.
#[instrument(skip(session, payload), fields(receiver = %receiver_path, identifier = %identifier))]
pub async fn upsert_payment_endpoint(
    session: &PubkySession,
    receiver_path: &PaykitReceiverPath,
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
) -> Result<()> {
    let path = payment_endpoint_path(receiver_path, identifier);
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
#[instrument(skip(session), fields(receiver = %receiver_path, identifier = %identifier))]
pub async fn delete_payment_endpoint(
    session: &PubkySession,
    receiver_path: &PaykitReceiverPath,
    identifier: &PaymentEndpointIdentifier,
) -> Result<()> {
    let path = payment_endpoint_path(receiver_path, identifier);
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
#[instrument(skip(storage), fields(payee = %payee, receiver = %receiver_path))]
pub async fn fetch_payment_list(
    storage: &PublicStorage,
    payee: &PublicKey,
    receiver_path: &PaykitReceiverPath,
) -> Result<PaymentList> {
    let addr = format!("{payee}{}", payment_endpoint_path_prefix(receiver_path));
    debug!(addr = %addr, "listing Payment Endpoints");
    fetch_payment_list_from_directory(storage, addr).await
}

/// Lists public Paykit receiver paths published by an identity.
#[instrument(skip(storage), fields(owner = %owner))]
pub async fn fetch_paykit_receiver_paths(
    storage: &PublicStorage,
    owner: &PublicKey,
) -> Result<Vec<PaykitReceiverPath>> {
    let addr = format!("{owner}{}", receiver_path_prefix());
    debug!(addr = %addr, "listing Paykit receivers");
    let app_resources = list_resources(storage, addr, "list Paykit receiver apps").await?;
    let mut receiver_paths = Vec::new();
    let mut seen_receiver_paths = HashSet::new();

    for resource in app_resources {
        let path = resource.path.as_str();
        let Some(app_segment) = listed_child_segment(path, &receiver_path_prefix()) else {
            debug!(path = %path, "skipping Paykit app path with unexpected shape");
            continue;
        };

        if app_segment == "private" {
            continue;
        }

        let runtime_addr = format!("{owner}{PAYKIT_PATH_PREFIX}/{app_segment}/");
        let runtime_resources =
            list_resources(storage, runtime_addr, "list Paykit receiver runtimes").await?;

        for runtime_resource in runtime_resources {
            let runtime_path = runtime_resource.path.as_str();
            let app_prefix = format!("{PAYKIT_PATH_PREFIX}/{app_segment}/");
            let Some(runtime_segment) = listed_child_segment(runtime_path, &app_prefix) else {
                debug!(path = %runtime_path, "skipping Paykit runtime path with unexpected shape");
                continue;
            };
            let receiver_text = format!("{app_segment}/{runtime_segment}");
            let receiver_path = match PaykitReceiverPath::new(&receiver_text) {
                Ok(receiver_path) => receiver_path,
                Err(err) => {
                    debug!(
                        path = %runtime_path,
                        receiver_path = %receiver_text,
                        error = %err,
                        "skipping invalid Paykit receiver path from directory listing"
                    );
                    continue;
                }
            };
            if seen_receiver_paths.insert(receiver_path.clone()) {
                receiver_paths.push(receiver_path);
            }
        }
    }

    receiver_paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    Ok(receiver_paths)
}

/// Fetches an individual receiver-scoped public Payment Endpoint.
#[instrument(skip(storage), fields(payee = %payee, receiver = %receiver_path, identifier = %identifier))]
pub async fn fetch_payment_endpoint(
    storage: &PublicStorage,
    payee: &PublicKey,
    receiver_path: &PaykitReceiverPath,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    let addr = format!(
        "{payee}{}",
        payment_endpoint_path(receiver_path, identifier)
    );
    debug!(addr = %addr, "fetching Payment Endpoint");
    match fetch_text(storage, addr, "fetch Payment Endpoint").await? {
        Some(payload) => Ok(Some(PaymentEndpointPayload::new(payload))),
        None => Ok(None),
    }
}

/// Return the receiver registry path prefix.
pub(crate) fn receiver_path_prefix() -> String {
    format!("{PAYKIT_PATH_PREFIX}/")
}

/// Return the receiver-scoped public Payment Endpoint path prefix.
pub(crate) fn payment_endpoint_path_prefix(receiver_path: &PaykitReceiverPath) -> String {
    format!("{PAYKIT_PATH_PREFIX}/{receiver_path}/endpoints/")
}

/// Return the receiver-scoped public Payment Endpoint path.
pub(crate) fn payment_endpoint_path(
    receiver_path: &PaykitReceiverPath,
    identifier: &PaymentEndpointIdentifier,
) -> String {
    format!(
        "{}{}",
        payment_endpoint_path_prefix(receiver_path),
        identifier.as_str()
    )
}

/// Return the receiver-scoped private message base path.
pub(crate) fn private_message_path_prefix(receiver_path: &PaykitReceiverPath) -> String {
    format!("{PAYKIT_PRIVATE_PATH_PREFIX}/{receiver_path}/messages")
}

/// Return the receiver-scoped Receipt Location prefix.
pub(crate) fn receipt_path_prefix(receiver_path: &PaykitReceiverPath) -> String {
    format!("{PAYKIT_PRIVATE_PATH_PREFIX}/{receiver_path}/receipts")
}

/// Return the receiver-scoped Encrypted Link recovery marker base path.
pub(crate) fn encrypted_link_recovery_path_prefix(receiver_path: &PaykitReceiverPath) -> String {
    format!("{PAYKIT_PRIVATE_PATH_PREFIX}/{receiver_path}/encrypted-link-recovery")
}

pub(crate) fn receiver_pair_path_domain(
    base_domain: &[u8],
    local_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_public_key: &PublicKey,
    remote_receiver_path: &PaykitReceiverPath,
) -> Vec<u8> {
    let mut endpoints = [
        (
            local_public_key.z32(),
            local_receiver_path.as_str().to_owned(),
        ),
        (
            remote_public_key.z32(),
            remote_receiver_path.as_str().to_owned(),
        ),
    ];
    endpoints.sort();

    let mut domain = Vec::with_capacity(
        base_domain.len()
            + endpoints
                .iter()
                .map(|(public_key, receiver_path)| public_key.len() + receiver_path.len() + 2)
                .sum::<usize>()
            + 1,
    );
    domain.extend_from_slice(base_domain);
    for (public_key, receiver_path) in endpoints {
        domain.push(0);
        domain.extend_from_slice(public_key.as_bytes());
        domain.push(0);
        domain.extend_from_slice(receiver_path.as_bytes());
    }
    domain
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

fn listed_child_segment<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let suffix = path.strip_prefix(prefix)?;
    suffix
        .trim_end_matches('/')
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
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
        let receiver_path = PaykitReceiverPath::new("bitkit/wallet").unwrap();
        let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();

        assert_eq!(
            payment_endpoint_path(&receiver_path, &identifier),
            "/pub/paykit/v0/bitkit/wallet/endpoints/btc-lightning-bolt11"
        );
        assert_eq!(receiver_path_prefix(), "/pub/paykit/v0/");
        assert_eq!(
            private_message_path_prefix(&receiver_path),
            "/pub/paykit/v0/private/bitkit/wallet/messages"
        );
        assert_eq!(
            receipt_path_prefix(&receiver_path),
            "/pub/paykit/v0/private/bitkit/wallet/receipts"
        );
        assert_eq!(
            encrypted_link_recovery_path_prefix(&receiver_path),
            "/pub/paykit/v0/private/bitkit/wallet/encrypted-link-recovery"
        );
    }

    #[test]
    fn test_listed_child_segment_returns_direct_child() {
        assert_eq!(
            listed_child_segment("/pub/paykit/v0/bitkit/", "/pub/paykit/v0/"),
            Some("bitkit")
        );
        assert_eq!(
            listed_child_segment("/pub/paykit/v0/bitkit/wallet/", "/pub/paykit/v0/bitkit/"),
            Some("wallet")
        );
        assert_eq!(
            listed_child_segment(
                "/pub/paykit/v0/bitkit/wallet/endpoints/x",
                "/pub/paykit/v0/"
            ),
            Some("bitkit")
        );
    }
}
