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
    parse_paykit_app_registry_json, serialize_paykit_app_registry, PaykitAppId, PaykitAppRegistry,
    PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList, Result,
    PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES, PAYMENT_LIST_MAX_ENDPOINTS,
};

/// Conventional prefix for public Paykit data hosted on Pubky storage.
///
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";

/// Public path for the identity-wide Paykit App Registry.
pub const PAYKIT_APP_REGISTRY_PATH: &str = "/pub/paykit/v0/app-registry.json";

/// Pubky path for the encrypted identity-wide SDK state.
pub const PAYKIT_SHARED_STATE_PATH: &str = "/pub/paykit/v0/shared-state.bin";

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

const LIST_PAGE_LIMIT: u16 = 100;

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
    session
        .storage()
        .put(path, payload.as_str().to_string())
        .await
        .map_err(|err| {
            log_payment_endpoint_storage_failure("put", &err);
            PaykitError::Transport {
                context: "put endpoint".into(),
                source: err.into(),
            }
        })?;
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
    session
        .storage()
        .put_if_absent(path, payload.as_str().to_string())
        .await
        .map_err(|err| PaykitError::Transport {
            context: "create endpoint".into(),
            source: err.into(),
        })?;
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
    session
        .storage()
        .put_if_match(path, payload.as_str().to_string(), revision)
        .await
        .map_err(|err| PaykitError::Transport {
            context: "update endpoint".into(),
            source: err.into(),
        })?;
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
    match session.storage().delete(path).await {
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
    session
        .storage()
        .delete_if_match(path, revision)
        .await
        .map_err(|err| PaykitError::Transport {
            context: "delete endpoint".into(),
            source: err.into(),
        })?;
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
#[instrument(skip(storage), fields(payee = %payee))]
pub async fn fetch_payment_list_with_limits(
    storage: &PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
    max_endpoints: usize,
    max_total_payload_bytes: usize,
) -> Result<PaymentList> {
    let addr = format!("{payee}{}", payment_endpoint_path_prefix(app_id));
    debug!("listing payment endpoints");
    let resources = list_resources(storage, addr, "list payment endpoints").await?;

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
    let mut remaining_payload_bytes = max_total_payload_bytes;
    for resource in resources {
        if remaining_payload_bytes == 0 {
            return Err(payment_list_limit_exceeded(format!(
                "Payment List exceeds the allowed {max_total_payload_bytes} payload bytes"
            )));
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
        let payload_limit = remaining_payload_bytes.min(PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES);
        let payload =
            match fetch_text(storage, resource.to_string(), &label, Some(payload_limit)).await {
                Ok(payload) => payload,
                Err(error)
                    if payload_limit < PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES
                        && is_response_size_limit_exceeded(&error) =>
                {
                    return Err(payment_list_limit_exceeded(format!(
                        "Payment List exceeds the allowed {max_total_payload_bytes} payload bytes"
                    )));
                }
                Err(error) => return Err(error),
            };
        if let Some(payload) = payload {
            remaining_payload_bytes -= payload.len();
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
    let mut response = match storage.get(&addr).await {
        Ok(response) => response,
        Err(err) if is_not_found(&err) => return Ok(None),
        Err(err) => {
            return Err(PaykitError::Transport {
                context: "fetch endpoint".into(),
                source: err.into(),
            });
        }
    };
    let revision = pubky::ResourceStats::from_headers(response.headers())
        .etag
        .filter(|etag| !etag.starts_with("W/\"") && !etag.is_empty())
        .ok_or_else(|| PaykitError::InvalidData {
            context: "Payment Endpoint response is missing a strong ETag".into(),
            source: None,
        })?;
    let payload = read_text_response(
        &mut response,
        "fetch endpoint",
        Some(PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES),
    )
    .await?
    .map(PaymentEndpointPayload::new);
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
    session
        .storage()
        .put_if_absent(PAYKIT_APP_REGISTRY_PATH, body)
        .await
        .map_err(|err| PaykitError::Transport {
            context: "create Paykit App Registry".into(),
            source: err.into(),
        })?;
    Ok(())
}

/// Replaces the identity-wide Paykit App Registry at one exact revision.
pub async fn update_paykit_app_registry(
    session: &PubkySession,
    registry: &PaykitAppRegistry,
    etag: &str,
) -> Result<()> {
    let body = serialize_paykit_app_registry(registry)?;
    session
        .storage()
        .put_if_match(PAYKIT_APP_REGISTRY_PATH, body, etag)
        .await
        .map_err(|err| PaykitError::Transport {
            context: "update Paykit App Registry".into(),
            source: err.into(),
        })?;
    Ok(())
}

/// Fetches and parses the identity-wide Paykit App Registry.
pub async fn fetch_paykit_app_registry(
    storage: &PublicStorage,
    owner: &PublicKey,
) -> Result<Option<PaykitAppRegistry>> {
    let addr = format!("{owner}{PAYKIT_APP_REGISTRY_PATH}");
    fetch_text(
        storage,
        addr,
        "fetch Paykit App Registry",
        Some(crate::PAYKIT_APP_REGISTRY_MAX_BYTES),
    )
    .await?
    .map(|body| parse_paykit_app_registry_json(&body))
    .transpose()
}

/// Fetches and parses the identity-wide Paykit App Registry with its ETag.
pub async fn fetch_paykit_app_registry_with_etag(
    storage: &PublicStorage,
    owner: &PublicKey,
) -> Result<Option<(PaykitAppRegistry, String)>> {
    let addr = format!("{owner}{PAYKIT_APP_REGISTRY_PATH}");
    let mut response = match storage.get(&addr).await {
        Ok(response) => response,
        Err(err) if is_not_found(&err) => return Ok(None),
        Err(err) => {
            return Err(PaykitError::Transport {
                context: "fetch Paykit App Registry".into(),
                source: err.into(),
            });
        }
    };
    let etag = pubky::ResourceStats::from_headers(response.headers())
        .etag
        .filter(|etag| !etag.starts_with("W/\""))
        .ok_or_else(|| PaykitError::InvalidData {
            context: "Paykit App Registry response is missing a strong ETag".into(),
            source: None,
        })?;
    let body = read_text_response(
        &mut response,
        "fetch Paykit App Registry",
        Some(crate::PAYKIT_APP_REGISTRY_MAX_BYTES),
    )
    .await?
    .ok_or_else(|| PaykitError::InvalidData {
        context: "Paykit App Registry is empty".into(),
        source: None,
    })?;
    Ok(Some((parse_paykit_app_registry_json(&body)?, etag)))
}

#[instrument(skip(storage, addr, label), fields(operation = %label))]
pub(crate) async fn fetch_text(
    storage: &PublicStorage,
    addr: String,
    label: &str,
    max_bytes: Option<usize>,
) -> Result<Option<String>> {
    trace!("fetching text resource");
    match storage.get(&addr).await {
        Ok(mut resp) => read_text_response(&mut resp, label, max_bytes).await,
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

async fn read_text_response(
    response: &mut reqwest::Response,
    label: &str,
    max_bytes: Option<usize>,
) -> Result<Option<String>> {
    if let (Some(max_bytes), Some(content_length)) = (max_bytes, response.content_length()) {
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
        if let Some(max_bytes) = max_bytes {
            if bytes.len().saturating_add(chunk.len()) > max_bytes {
                return Err(PaykitError::InvalidData {
                    context: format!("{label}: response exceeds the {max_bytes}-byte limit"),
                    source: Some(ResponseSizeLimitExceeded.into()),
                });
            }
        }
        bytes.extend_from_slice(&chunk);
    }
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
