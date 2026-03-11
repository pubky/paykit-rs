//! Unauthenticated Pubky adapter that exposes reads over [`crate::UnauthenticatedTransportRead`].

use std::collections::HashMap;

use async_trait::async_trait;
use pubky::{
    errors::RequestError, Error as PubkyError, PubkyResource,
    PublicStorage as SdkUnauthenticatedTransport, StatusCode,
};
use tracing::{debug, error, instrument, trace};

use super::PAYKIT_PATH_PREFIX;
use crate::transport::traits::UnauthenticatedTransportRead;
use crate::{EndpointData, MethodId, PaykitError, PublicKey, Result, SupportedPayments};

/// Adapter around `pubky::PublicStorage` implementing `UnauthenticatedTransportRead`.
#[derive(Clone)]
pub struct PubkyUnauthenticatedTransport {
    inner: SdkUnauthenticatedTransport,
}

impl PubkyUnauthenticatedTransport {
    /// Build an adapter from an existing SDK handle.
    pub fn new(inner: SdkUnauthenticatedTransport) -> Self {
        debug!("creating PubkyUnauthenticatedTransport from existing handle");
        Self { inner }
    }

    /// Attempt to construct the underlying SDK transport via `pubky::PublicStorage::new()`.
    pub fn try_new() -> Result<Self> {
        debug!("attempting to create PubkyUnauthenticatedTransport via PublicStorage::new()");
        let inner = SdkUnauthenticatedTransport::new().map_err(|err| {
            error!(error = %err, "failed to create Pubky public transport");
            PaykitError::Transport {
                context: "failed to create Pubky public transport".into(),
                source: err.into(),
            }
        })?;
        debug!("PubkyUnauthenticatedTransport created successfully");
        Ok(Self { inner })
    }

    /// Access the wrapped SDK transport handle.
    pub fn inner(&self) -> &SdkUnauthenticatedTransport {
        &self.inner
    }

    #[instrument(skip(self), fields(addr = %addr, label = %label))]
    async fn fetch_text(&self, addr: String, label: &str) -> Result<Option<String>> {
        trace!("fetching text resource");
        match self.inner.get(&addr).await {
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

    #[instrument(skip(self), fields(addr = %addr, label = %label))]
    async fn list_entries(&self, addr: String, label: &str) -> Result<Vec<PubkyResource>> {
        trace!("listing directory entries");
        let builder = match self.inner.list(&addr) {
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
}

#[async_trait]
impl UnauthenticatedTransportRead for PubkyUnauthenticatedTransport {
    // NOTE: Race condition — the directory listing and subsequent per-entry
    // fetches are **not** atomic. Between the `list_entries` call and the
    // individual `fetch_text` calls the payee may add, remove, or update
    // endpoints. The returned `SupportedPayments` is therefore a best-effort
    // snapshot. The underlying Pubky storage layer does not expose
    // locks or transactional reads, so this cannot be resolved at the
    // transport level.
    //
    // If a payment execution error suggests the endpoint has already been
    // consumed (evidence of a race), callers should re-fetch the specific
    // endpoint via `fetch_payment_endpoint`, compare the `EndpointData`, and
    // retry with the updated value if it differs.
    #[instrument(skip(self), fields(payee = %payee))]
    async fn fetch_supported_payments(&self, payee: &PublicKey) -> Result<SupportedPayments> {
        let addr = format!("{payee}{PAYKIT_PATH_PREFIX}");
        debug!(addr = %addr, "listing supported payment methods");
        let entries = self.list_entries(addr, "list supported payments").await?;

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

            let label = format!("fetch endpoint {}", method);
            if let Some(payload) = self.fetch_text(resource.to_string(), &label).await? {
                debug!(method = %method, "fetched payment endpoint payload");
                let method_id = MethodId::new(&method).map_err(|err| PaykitError::InvalidData {
                    context: format!("storage returned invalid method identifier '{}'", method),
                    source: Some(err.into()),
                })?;
                map.insert(method_id, EndpointData::new(payload));
            }
        }

        debug!(count = map.len(), "supported payments collected");
        Ok(SupportedPayments { entries: map })
    }

    #[instrument(skip(self), fields(payee = %payee, method = %method))]
    async fn fetch_payment_endpoint(
        &self,
        payee: &PublicKey,
        method: &MethodId,
    ) -> Result<Option<EndpointData>> {
        let addr = format!("{payee}{PAYKIT_PATH_PREFIX}{}", method.as_str());
        debug!(addr = %addr, "fetching individual payment endpoint");
        match self.fetch_text(addr, "fetch endpoint").await? {
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
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}
