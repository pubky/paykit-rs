//! Unauthenticated Pubky adapter that exposes reads over [`crate::UnauthenticatedTransportRead`].

use std::collections::HashMap;

use async_trait::async_trait;
use pubky::{
    errors::RequestError, Error as PubkyError, PubkyResource,
    PublicStorage as SdkUnauthenticatedTransport, StatusCode,
};
use tracing::{debug, error, instrument, trace, warn};

use pubky_app_specs::PubkyAppObject;

use super::{PAYKIT_PATH_PREFIX, PUBKY_FOLLOWS_PATH, PUBKY_PROFILE_FILE};
use crate::transport::traits::UnauthenticatedTransportRead;
use crate::{EndpointData, MethodId, PaykitError, Profile, PublicKey, Result, SupportedPayments};

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
            PaykitError::Transport(format!("failed to create Pubky public transport: {err}"))
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
                    PaykitError::Transport(format!("{label}: {err}"))
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
                    PaykitError::InvalidData(format!("{label}: invalid UTF-8 at byte {pos}"))
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
                Err(PaykitError::Transport(format!("{label}: {err}")))
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
                return Err(PaykitError::Transport(format!("{label}: {err}")));
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
                Err(PaykitError::Transport(format!(
                    "{label} send failed: {err}"
                )))
            }
        }
    }
}

#[async_trait]
impl UnauthenticatedTransportRead for PubkyUnauthenticatedTransport {
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
                    PaykitError::InvalidData(format!(
                        "cannot extract method from resource path '{}'",
                        resource.path
                    ))
                })?
                .to_string();

            let label = format!("fetch endpoint {}", method);
            if let Some(payload) = self.fetch_text(resource.to_string(), &label).await? {
                debug!(method = %method, "fetched payment endpoint payload");
                map.insert(MethodId(method), EndpointData(payload));
            }
        }

        debug!(count = map.len(), "supported payments collected");
        Ok(SupportedPayments { entries: map })
    }

    #[instrument(skip(self), fields(payee = %payee, method = %method.0))]
    async fn fetch_payment_endpoint(
        &self,
        payee: &PublicKey,
        method: &MethodId,
    ) -> Result<Option<EndpointData>> {
        let addr = format!("{payee}{PAYKIT_PATH_PREFIX}{}", method.0);
        debug!(addr = %addr, "fetching individual payment endpoint");
        match self.fetch_text(addr, "fetch endpoint").await? {
            Some(payload) => {
                debug!("payment endpoint found");
                Ok(Some(EndpointData(payload)))
            }
            None => {
                debug!("payment endpoint not found");
                Ok(None)
            }
        }
    }

    #[instrument(skip(self), fields(owner = %owner))]
    async fn fetch_known_contacts(&self, owner: &PublicKey) -> Result<Vec<PublicKey>> {
        let addr = format!("{owner}{PUBKY_FOLLOWS_PATH}");
        debug!(addr = %addr, "listing known contacts");
        let entries = self.list_entries(addr, "list known contacts").await?;

        let mut contacts = Vec::new();
        for resource in entries {
            if resource.path.as_str().ends_with('/') {
                trace!(path = %resource.path, "skipping directory entry");
                continue;
            }
            let name = resource
                .path
                .as_str()
                .rsplit('/')
                .next()
                .filter(|segment| !segment.is_empty());
            if let Some(pk_str) = name {
                match pk_str.parse::<PublicKey>() {
                    Ok(pk) => contacts.push(pk),
                    Err(err) => {
                        error!(entry = %pk_str, error = %err, "skipping invalid contact entry, cannot parse as PublicKey");
                        continue;
                    }
                }
            }
        }

        debug!(count = contacts.len(), "known contacts collected");
        Ok(contacts)
    }

    #[instrument(skip(self), fields(user = %user))]
    async fn fetch_profile(&self, user: &PublicKey) -> Result<Profile> {
        debug!("constructing profile resource");
        let resource = PubkyResource::new(user.clone(), PUBKY_PROFILE_FILE).map_err(|e| {
            error!(error = %e, "failed to construct profile resource");
            PaykitError::Transport(format!(
                "failed to construct profile resource for {user}: {e}"
            ))
        })?;

        debug!("fetching profile blob from storage");
        let blob = match self.inner.get(&resource).await {
            Ok(resp) => resp
                .bytes()
                .await
                .map_err(|err| {
                    error!(error = %err, "failed to read profile response bytes");
                    PaykitError::Transport(format!("fetch profile bytes failed: {err}"))
                })?
                .to_vec(),
            Err(err) if is_not_found(&err) => {
                debug!("profile not found (404/GONE)");
                return Err(PaykitError::NotFound("profile not found".into()));
            }
            Err(err) => {
                error!(error = %err, "transport error fetching profile");
                return Err(PaykitError::Transport(format!(
                    "fetch profile failed: {err}"
                )));
            }
        };

        debug!(blob_len = blob.len(), "parsing profile blob");
        match PubkyAppObject::from_uri(&resource.to_pubky_url(), &blob) {
            Ok(PubkyAppObject::User(profile)) => {
                debug!("profile parsed successfully");
                Ok(profile)
            }
            Ok(_) => {
                warn!("resource exists but is not a user profile");
                Err(PaykitError::Profile(
                    "resource is not a user profile".into(),
                ))
            }
            Err(e) => {
                warn!(error = %e, "failed to parse profile data");
                Err(PaykitError::Profile(format!(
                    "failed to parse profile: {e}"
                )))
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
