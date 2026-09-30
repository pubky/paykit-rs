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
    parse_paykit_app_registry_json, serialize_paykit_app_registry, validation::invalid_data,
    PaykitAppId, PaykitAppRegistry, PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentList, Result, PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES, PAYMENT_LIST_MAX_ENDPOINTS,
};

mod locks;
pub use locks::{
    delete_resource_if_revision, is_write_conflict, put_resource_if_revision, with_write_lock,
};

/// Content fingerprint used to detect an application-level stale edit.
pub fn content_revision(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// Conventional prefix for public Paykit data hosted on Pubky storage.
///
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";

/// Public path for the identity-wide Paykit App Registry.
pub const PAYKIT_APP_REGISTRY_PATH: &str = "/pub/paykit/v0/app-registry.json";

/// Pubky path for the encrypted identity-wide SDK state.
pub const PAYKIT_SHARED_STATE_PATH: &str = "/pub/paykit/v0/shared-state.bin";

/// Prefix for empty markers identifying unconfirmed shared-state writes.
pub const PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX: &str = "/pub/paykit/v0/shared-state-writes/";

/// Conventional prefix for private (encrypted) Paykit data.
///
/// This prefix is used as the base path for pubky-noise's encrypted messaging.
/// The actual write and read paths are derived per-counterparty pair using
/// [`pubky_noise::path_derivation::derive_asymmetric_paths`]. Pubky-noise manages
/// individual file slots within the derived folders using a counter-based scheme.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";

/// Conventional prefix for Encrypted Link recovery markers.
///
/// Marker paths are derived per-counterparty pair before being appended below
/// this prefix, so the prefix itself does not identify the counterparty pair.
pub const PAYKIT_ENCRYPTED_LINK_RECOVERY_PATH_PREFIX: &str =
    "/pub/paykit/v0/encrypted-link-recovery";

pub(crate) fn identity_pair_path_domain(
    domain: &[u8],
    local_identity_public_key: &PublicKey,
    remote_identity_public_key: &PublicKey,
) -> Vec<u8> {
    let mut identities = [
        local_identity_public_key.to_bytes(),
        remote_identity_public_key.to_bytes(),
    ];
    identities.sort_unstable();
    // Fixed-width keys make the pair unambiguous; sorting preserves peer parity.
    // The caller still derives paths with a Noise DH secret to keep them private.
    let mut path_domain = Vec::with_capacity(domain.len() + 64);
    path_domain.extend_from_slice(domain);
    path_domain.extend_from_slice(&identities[0]);
    path_domain.extend_from_slice(&identities[1]);
    path_domain
}

const LIST_PAGE_LIMIT: u16 = 100;
const LIST_MAX_PAGES: usize = 100;

/// Maximum accepted size for a public Paykit document (Payment Endpoint
/// payloads, the Paykit App Registry, and recovery markers).
///
/// Public objects are served by other users' homeservers and must not be
/// trusted to fit in memory; an unbounded read lets a hostile publisher
/// exhaust the client. 64 KiB is far above any legitimate descriptor
/// (LNURL, BOLT11 invoice, JSON endpoint payload).
pub(crate) const MAX_PUBLIC_RESOURCE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("Payment List exceeds caller-supplied limits")]
struct PaymentListLimitExceeded;

#[derive(Debug, thiserror::Error)]
#[error("response exceeds caller-supplied byte limit")]
struct ResponseSizeLimitExceeded;

pub(crate) fn is_payment_list_limit_exceeded(error: &PaykitError) -> bool {
    matches!(
        error,
        PaykitError::InvalidData {
            source: Some(source),
            ..
        } if source.downcast_ref::<PaymentListLimitExceeded>().is_some()
    )
}

fn is_response_size_limit_exceeded(error: &PaykitError) -> bool {
    matches!(
        error,
        PaykitError::InvalidData {
            source: Some(source),
            ..
        } if source.downcast_ref::<ResponseSizeLimitExceeded>().is_some()
    )
}

fn payment_list_limit_exceeded(context: String) -> PaykitError {
    PaykitError::InvalidData {
        context,
        source: Some(PaymentListLimitExceeded.into()),
    }
}

pub(crate) fn log_payment_endpoint_storage_failure(
    operation: &'static str,
    _error: &impl std::fmt::Display,
) {
    error!(operation, "payment endpoint storage request failed");
}

/// Writes or updates a payment endpoint document in the authenticated Pubky session.
#[instrument(skip(session, payload), fields(identifier = %identifier))]
pub async fn upsert_payment_endpoint(
    session: &PubkySession,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
) -> Result<()> {
    validate_payment_endpoint_payload(payload)?;
    let path = payment_endpoint_path(app_id, identifier);
    debug!(path = %path, "writing payment endpoint to Pubky storage");
    with_write_lock(session, &path, |lock| async move {
        session
            .storage()
            .put_locked(&lock, payload.as_str().to_string())
            .await
            .map_err(|err| {
                log_payment_endpoint_storage_failure("put", &err);
                PaykitError::Transport {
                    context: "put endpoint".into(),
                    source: err.into(),
                }
            })
            .map(|_| ())
    })
    .await?;
    debug!("payment endpoint stored successfully");
    Ok(())
}

pub async fn create_payment_endpoint(
    session: &PubkySession,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
) -> Result<()> {
    validate_payment_endpoint_payload(payload)?;
    let path = payment_endpoint_path(app_id, identifier);
    put_resource_if_revision(
        session,
        &path,
        payload.as_str().as_bytes().to_vec(),
        None,
        PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES,
    )
    .await?;
    Ok(())
}

pub async fn update_payment_endpoint(
    session: &PubkySession,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
    payload: &PaymentEndpointPayload,
    revision: &str,
) -> Result<()> {
    validate_payment_endpoint_payload(payload)?;
    let path = payment_endpoint_path(app_id, identifier);
    put_resource_if_revision(
        session,
        &path,
        payload.as_str().as_bytes().to_vec(),
        Some(revision),
        PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES,
    )
    .await?;
    Ok(())
}

/// Removes an existing payment endpoint from the authenticated Pubky session.
#[instrument(skip(session), fields(identifier = %identifier))]
pub async fn delete_payment_endpoint(
    session: &PubkySession,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
) -> Result<()> {
    let path = payment_endpoint_path(app_id, identifier);
    debug!(path = %path, "deleting payment endpoint from Pubky storage");
    with_write_lock(session, &path, |lock| async move {
        match session.storage().delete_locked(&lock).await {
            Ok(_) => {}
            Err(err) if is_not_found(&err) => {
                debug!("payment endpoint already absent");
            }
            Err(err) => {
                log_payment_endpoint_storage_failure("delete", &err);
                return Err(PaykitError::Transport {
                    context: "delete endpoint".into(),
                    source: err.into(),
                });
            }
        }
        Ok::<_, PaykitError>(())
    })
    .await?;
    debug!("payment endpoint removed successfully");
    Ok(())
}

pub async fn delete_payment_endpoint_if_revision(
    session: &PubkySession,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
    revision: &str,
) -> Result<()> {
    let path = payment_endpoint_path(app_id, identifier);
    delete_resource_if_revision(session, &path, revision, PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES)
        .await?;
    Ok(())
}

fn validate_payment_endpoint_payload(payload: &PaymentEndpointPayload) -> Result<()> {
    if payload.as_str().len() > PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES {
        return Err(PaykitError::Validation(format!(
            "Payment Endpoint payload must not exceed {PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Fetches all public payment endpoints for the provided payee from Pubky storage.
///
/// Directory listing and per-resource fetches are not atomic; the returned list is a
/// best-effort snapshot of the payee's homeserver state.
#[instrument(skip(storage, remaining_endpoints, remaining_requests, remaining_payload_bytes), fields(payee = %payee))]
pub async fn fetch_payment_list_with_budget(
    storage: &PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
    remaining_endpoints: &mut usize,
    remaining_requests: &mut usize,
    remaining_payload_bytes: &mut usize,
) -> Result<PaymentList> {
    let max_endpoints = (*remaining_endpoints).min(PAYMENT_LIST_MAX_ENDPOINTS);
    let addr = format!("{payee}{}", payment_endpoint_path_prefix(app_id));
    debug!("listing payment endpoints");
    let resources =
        list_resources(storage, addr, "list payment endpoints", remaining_requests).await?;

    let resources = resources
        .into_iter()
        .filter(|resource| !resource.path.as_str().ends_with('/'))
        .collect::<Vec<_>>();
    if resources.len() > max_endpoints {
        let context =
            format!("Payment List contains more than the allowed {max_endpoints} endpoints");
        return if max_endpoints < PAYMENT_LIST_MAX_ENDPOINTS {
            Err(payment_list_limit_exceeded(context))
        } else {
            Err(PaykitError::InvalidData {
                context,
                source: None,
            })
        };
    }

    let mut map = HashMap::new();
    for resource in resources {
        if *remaining_payload_bytes == 0 {
            return Err(payment_list_limit_exceeded(
                "Payment List payload budget exhausted".into(),
            ));
        }

        let identifier_text = resource
            .path
            .as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or_else(|| {
                error!("invalid resource path for Payment Endpoint");
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
        let payload_limit = (*remaining_payload_bytes).min(PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES);
        consume_payment_list_request(remaining_requests)?;
        *remaining_endpoints -= 1;
        let payload = match fetch_text_with_budget(
            storage,
            resource.to_string(),
            &label,
            Some(payload_limit),
            remaining_payload_bytes,
        )
        .await
        {
            Ok(payload) => payload,
            Err(error)
                if payload_limit < PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES
                    && is_response_size_limit_exceeded(&error) =>
            {
                return Err(payment_list_limit_exceeded(
                    "Payment List payload budget exhausted".into(),
                ));
            }
            Err(error) => return Err(error),
        };
        if let Some(payload) = payload {
            debug!(identifier = %identifier_text, "fetched Payment Endpoint Payload");
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

    debug!(count = map.len(), "Payment List collected");
    Ok(PaymentList {
        payment_endpoints: map,
    })
}

fn consume_payment_list_request(remaining_requests: &mut usize) -> Result<()> {
    *remaining_requests = remaining_requests.checked_sub(1).ok_or_else(|| {
        payment_list_limit_exceeded("Payment List request budget exhausted".into())
    })?;
    Ok(())
}

/// Fetches an individual public payment endpoint from Pubky storage.
#[instrument(skip(storage), fields(payee = %payee, identifier = %identifier))]
pub async fn fetch_payment_endpoint(
    storage: &PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    let addr = format!("{payee}{}", payment_endpoint_path(app_id, identifier));
    debug!("fetching individual payment endpoint");
    match fetch_text(
        storage,
        addr,
        "fetch endpoint",
        Some(PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES),
    )
    .await?
    {
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

pub async fn fetch_payment_endpoint_with_revision(
    storage: &PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<(Option<PaymentEndpointPayload>, String)>> {
    let addr = format!("{payee}{}", payment_endpoint_path(app_id, identifier));
    let response = match storage.get(&addr).await {
        Ok(response) => response,
        Err(err) if is_not_found(&err) => return Ok(None),
        Err(err) => {
            return Err(PaykitError::Transport {
                context: "fetch endpoint".into(),
                source: err.into(),
            });
        }
    };
    let bytes = read_bounded_body(
        response,
        PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES,
        "fetch endpoint",
    )
    .await?;
    let revision = content_revision(&bytes);
    let payload = String::from_utf8(bytes)
        .map_err(|err| invalid_data("Payment Endpoint is not UTF-8", Some(err.into())))?;
    let payload = if payload.is_empty() {
        None
    } else {
        Some(PaymentEndpointPayload::new(payload))
    };
    Ok(Some((payload, revision)))
}

pub(crate) fn payment_endpoint_path_prefix(app_id: &PaykitAppId) -> String {
    format!("{PAYKIT_PATH_PREFIX}apps/{app_id}/endpoints/")
}

pub(crate) fn payment_endpoint_path(
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
) -> String {
    format!(
        "{}{}",
        payment_endpoint_path_prefix(app_id),
        identifier.as_str()
    )
}

/// Creates the identity-wide Paykit App Registry if it does not exist.
pub async fn create_paykit_app_registry(
    session: &PubkySession,
    registry: &PaykitAppRegistry,
) -> Result<()> {
    let body = serialize_paykit_app_registry(registry)?;
    put_resource_if_revision(
        session,
        PAYKIT_APP_REGISTRY_PATH,
        body.into_bytes(),
        None,
        crate::PAYKIT_APP_REGISTRY_MAX_BYTES,
    )
    .await?;
    Ok(())
}

/// Replaces the identity-wide Paykit App Registry at one exact revision.
pub async fn update_paykit_app_registry(
    session: &PubkySession,
    registry: &PaykitAppRegistry,
    revision: &str,
) -> Result<()> {
    let body = serialize_paykit_app_registry(registry)?;
    put_resource_if_revision(
        session,
        PAYKIT_APP_REGISTRY_PATH,
        body.into_bytes(),
        Some(revision),
        crate::PAYKIT_APP_REGISTRY_MAX_BYTES,
    )
    .await?;
    Ok(())
}

/// Fetches and parses the identity-wide Paykit App Registry.
pub async fn fetch_paykit_app_registry(
    storage: &PublicStorage,
    owner: &PublicKey,
) -> Result<Option<PaykitAppRegistry>> {
    Ok(fetch_paykit_app_registry_with_revision(storage, owner)
        .await?
        .map(|(registry, _)| registry))
}

/// Fetches and parses the identity-wide Paykit App Registry with its content revision.
pub async fn fetch_paykit_app_registry_with_revision(
    storage: &PublicStorage,
    owner: &PublicKey,
) -> Result<Option<(PaykitAppRegistry, String)>> {
    let addr = format!("{owner}{PAYKIT_APP_REGISTRY_PATH}");
    let response = match storage.get(&addr).await {
        Ok(response) => response,
        Err(err) if is_not_found(&err) => return Ok(None),
        Err(err) => {
            return Err(PaykitError::Transport {
                context: "fetch Paykit App Registry".into(),
                source: err.into(),
            });
        }
    };
    let bytes = read_bounded_body(
        response,
        crate::PAYKIT_APP_REGISTRY_MAX_BYTES,
        "fetch Paykit App Registry",
    )
    .await?;
    let revision = content_revision(&bytes);
    let body = String::from_utf8(bytes)
        .map_err(|err| invalid_data("Paykit App Registry is not UTF-8", Some(err.into())))?;
    Ok(Some((parse_paykit_app_registry_json(&body)?, revision)))
}

#[instrument(skip(storage, addr, label), fields(operation = %label))]
pub(crate) async fn fetch_text(
    storage: &PublicStorage,
    addr: String,
    label: &str,
    max_bytes: Option<usize>,
) -> Result<Option<String>> {
    let mut remaining_payload_bytes = usize::MAX;
    fetch_text_with_budget(
        storage,
        addr,
        label,
        max_bytes,
        &mut remaining_payload_bytes,
    )
    .await
}

async fn fetch_text_with_budget(
    storage: &PublicStorage,
    addr: String,
    label: &str,
    max_bytes: Option<usize>,
    remaining_payload_bytes: &mut usize,
) -> Result<Option<String>> {
    trace!("fetching text resource");
    match storage.get(&addr).await {
        Ok(mut resp) => {
            read_text_response(&mut resp, label, max_bytes, remaining_payload_bytes).await
        }
        Err(err) if is_not_found(&err) => {
            debug!("resource not found (404/GONE)");
            Ok(None)
        }
        Err(err) => {
            error!("transport error during fetch");
            Err(PaykitError::Transport {
                context: label.to_string(),
                source: err.into(),
            })
        }
    }
}

/// Read a response with a hard streaming bound, including when Content-Length
/// is absent or inaccurate. Outbox bodies are also untrusted homeserver data.
pub(crate) async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>> {
    read_response_body(&mut response, label, max_bytes).await
}

async fn read_response_body(
    response: &mut reqwest::Response,
    label: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut remaining_payload_bytes = usize::MAX;
    read_response_body_with_budget(response, label, max_bytes, &mut remaining_payload_bytes).await
}

async fn read_response_body_with_budget(
    response: &mut reqwest::Response,
    label: &str,
    max_bytes: usize,
    remaining_payload_bytes: &mut usize,
) -> Result<Vec<u8>> {
    if let Some(content_length) = response.content_length() {
        if content_length > max_bytes as u64 {
            return Err(PaykitError::InvalidData {
                context: format!("{label}: response exceeds the {max_bytes}-byte limit"),
                source: Some(ResponseSizeLimitExceeded.into()),
            });
        }
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        error!("failed to read response bytes");
        PaykitError::Transport {
            context: label.to_string(),
            source: err.into(),
        }
    })? {
        *remaining_payload_bytes = remaining_payload_bytes.saturating_sub(chunk.len());
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(PaykitError::InvalidData {
                context: format!("{label}: response exceeds the {max_bytes}-byte limit"),
                source: Some(ResponseSizeLimitExceeded.into()),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_text_response(
    response: &mut reqwest::Response,
    label: &str,
    max_bytes: Option<usize>,
    remaining_payload_bytes: &mut usize,
) -> Result<Option<String>> {
    let bytes = read_response_body_with_budget(
        response,
        label,
        max_bytes.unwrap_or(MAX_PUBLIC_RESOURCE_BYTES),
        remaining_payload_bytes,
    )
    .await?;
    if bytes.is_empty() {
        debug!("resource is empty, returning None");
        return Ok(None);
    }
    let data = String::from_utf8(bytes).map_err(|err| {
        let pos = err.utf8_error().valid_up_to();
        error!(
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

#[instrument(skip(storage, addr, label), fields(operation = %label))]
async fn list_resources(
    storage: &PublicStorage,
    addr: String,
    label: &str,
    remaining_requests: &mut usize,
) -> Result<Vec<PubkyResource>> {
    trace!("listing directory resources");
    let mut resources = Vec::new();
    let mut cursor = None::<String>;
    let mut pages = 0usize;

    loop {
        let mut builder = match storage.list(&addr) {
            Ok(builder) => builder.shallow(true).limit(LIST_PAGE_LIMIT),
            Err(err) if is_not_found(&err) => {
                debug!("directory not found, returning listed resources");
                return Ok(resources);
            }
            Err(err) => {
                error!("failed to create list builder");
                return Err(PaykitError::Transport {
                    context: label.to_string(),
                    source: err.into(),
                });
            }
        };

        if let Some(cursor) = cursor.as_deref() {
            builder = builder.cursor(cursor);
        }

        consume_payment_list_request(remaining_requests)?;
        let page = match builder.send().await {
            Ok(page) => page,
            Err(err) if is_not_found(&err) => {
                debug!("directory not found during send, returning listed resources");
                return Ok(resources);
            }
            Err(err) => {
                error!("list send failed");
                return Err(PaykitError::Transport {
                    context: format!("{label} send failed"),
                    source: err.into(),
                });
            }
        };

        if page.is_empty() {
            break;
        }

        pages += 1;
        if pages > LIST_MAX_PAGES {
            return Err(invalid_data(
                format!("{label}: listing exceeded {LIST_MAX_PAGES} pages"),
                None,
            ));
        }

        let page_len = page.len();
        if resources.len().saturating_add(page_len) > PAYMENT_LIST_MAX_ENDPOINTS {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "{label}: directory contains more than {PAYMENT_LIST_MAX_ENDPOINTS} resources"
                ),
                source: None,
            });
        }
        let next_cursor = page
            .last()
            .map(|resource| format!("{}{}", resource.owner.z32(), resource.path.as_str()))
            .ok_or_else(|| PaykitError::InvalidData {
                context: format!("{label}: non-empty page has no cursor resource"),
                source: None,
            })?;
        if cursor
            .as_ref()
            .is_some_and(|previous| next_cursor.as_str() <= previous.as_str())
        {
            return Err(PaykitError::InvalidData {
                context: format!("{label}: directory cursor did not advance"),
                source: None,
            });
        }
        cursor = Some(next_cursor);
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
mod tests;
