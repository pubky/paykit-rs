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
use crate::transport::policy::{execute_with_policy, TransportPolicy};
use crate::transport::traits::UnauthenticatedTransportRead;
use crate::{EndpointData, MethodId, PaykitError, Profile, PublicKey, Result, SupportedPayments};

/// Adapter around `pubky::PublicStorage` implementing `UnauthenticatedTransportRead`.
///
/// Every instance carries a [`TransportPolicy`] that governs timeout and retry
/// behaviour. The default policy (30 s timeout, 3 retries with exponential
/// backoff) is applied automatically — use [`with_policy`](Self::with_policy)
/// only when you need non-default settings.
#[derive(Clone)]
pub struct PubkyUnauthenticatedTransport {
    inner: SdkUnauthenticatedTransport,
    policy: TransportPolicy,
}

impl PubkyUnauthenticatedTransport {
    /// Build an adapter from an existing SDK handle.
    ///
    /// Uses [`TransportPolicy::default()`] (30 s timeout, 3 retries).
    pub fn new(inner: SdkUnauthenticatedTransport) -> Self {
        debug!("creating PubkyUnauthenticatedTransport from existing handle");
        Self {
            inner,
            policy: TransportPolicy::default(),
        }
    }

    /// Attempt to construct the underlying SDK transport via `pubky::PublicStorage::new()`.
    ///
    /// Uses [`TransportPolicy::default()`] (30 s timeout, 3 retries).
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
        Ok(Self {
            inner,
            policy: TransportPolicy::default(),
        })
    }

    /// Override the transport policy.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::time::Duration;
    /// # use paykit_lib::{PubkyUnauthenticatedTransport, TransportPolicy};
    /// let reader = PubkyUnauthenticatedTransport::try_new()
    ///     .unwrap()
    ///     .with_policy(TransportPolicy::builder()
    ///         .timeout(Duration::from_secs(5))
    ///         .max_retries(1)
    ///         .build());
    /// ```
    pub fn with_policy(mut self, policy: TransportPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Access the wrapped SDK transport handle.
    pub fn inner(&self) -> &SdkUnauthenticatedTransport {
        &self.inner
    }

    /// Access the current transport policy.
    pub fn policy(&self) -> &TransportPolicy {
        &self.policy
    }

    #[instrument(skip(self), fields(addr = %addr, label = %label))]
    async fn fetch_text(&self, addr: String, label: &str) -> Result<Option<String>> {
        trace!("fetching text resource");
        execute_with_policy(&self.policy, label, || {
            let addr = addr.clone();
            let label = label.to_string();
            async move {
                match self.inner.get(&addr).await {
                    Ok(resp) => {
                        let bytes = resp.bytes().await.map_err(|err| {
                            error!(error = %err, "failed to read response bytes");
                            PaykitError::Transport {
                                context: label.clone(),
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
                            context: label.clone(),
                            source: err.into(),
                        })
                    }
                }
            }
        })
        .await
    }

    #[instrument(skip(self), fields(addr = %addr, label = %label))]
    async fn list_entries(&self, addr: String, label: &str) -> Result<Vec<PubkyResource>> {
        trace!("listing directory entries");
        execute_with_policy(&self.policy, label, || {
            let addr = addr.clone();
            let label = label.to_string();
            async move {
                let builder = match self.inner.list(&addr) {
                    Ok(builder) => builder,
                    Err(err) if is_not_found(&err) => {
                        debug!("directory not found, returning empty list");
                        return Ok(Vec::new());
                    }
                    Err(err) => {
                        error!(error = %err, "failed to create list builder");
                        return Err(PaykitError::Transport {
                            context: label.clone(),
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
        })
        .await
    }

    /// Fetch the raw profile blob from storage, with policy applied.
    ///
    /// Returns the [`PubkyResource`] handle and raw bytes on success, so the
    /// caller can use the resource for parsing without reconstructing it.
    /// Parsing is handled by the caller so that non-retryable parse failures
    /// are not wrapped in the retry loop.
    #[instrument(skip(self), fields(user = %user))]
    async fn fetch_profile_blob(&self, user: &PublicKey) -> Result<(PubkyResource, Vec<u8>)> {
        debug!("constructing profile resource");
        let resource = PubkyResource::new(user.clone(), PUBKY_PROFILE_FILE).map_err(|e| {
            error!(error = %e, "failed to construct profile resource");
            PaykitError::Transport {
                context: format!("failed to construct profile resource for {user}"),
                source: e.into(),
            }
        })?;

        debug!("fetching profile blob from storage");
        let blob = execute_with_policy(&self.policy, "fetch_profile", || {
            let resource = resource.clone();
            async move {
                match self.inner.get(&resource).await {
                    Ok(resp) => {
                        let blob = resp
                            .bytes()
                            .await
                            .map_err(|err| {
                                error!(error = %err, "failed to read profile response bytes");
                                PaykitError::Transport {
                                    context: "fetch profile bytes failed".into(),
                                    source: err.into(),
                                }
                            })?
                            .to_vec();
                        Ok(blob)
                    }
                    Err(err) if is_not_found(&err) => {
                        debug!("profile not found (404/GONE)");
                        Err(PaykitError::NotFound("profile not found".into()))
                    }
                    Err(err) => {
                        error!(error = %err, "transport error fetching profile");
                        Err(PaykitError::Transport {
                            context: "fetch profile failed".into(),
                            source: err.into(),
                        })
                    }
                }
            }
        })
        .await?;

        Ok((resource, blob))
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
        let (resource, blob) = self.fetch_profile_blob(user).await?;

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
