//! Concrete Pubky public storage operations for Paykit Pubky Routing.

use std::collections::HashMap;

use pubky::{
    errors::RequestError, Error as PubkyError, PubkyResource, PubkySession, PublicStorage,
    StatusCode,
};
use tracing::{debug, error, instrument, trace};

use super::paths::{PublicPaymentEndpointPath, PublicPaymentListPath, ReceiptPayloadPath};
use crate::{
    PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList, PaymentReference,
    PublicKey, Result,
};

/// Build a write-side Pubky Routing Adapter over an authenticated Pubky session.
pub(crate) fn for_session(session: &PubkySession) -> PublicPaymentStorage<'_> {
    PublicPaymentStorage { session }
}

/// Build a read-side Pubky Routing Adapter over Pubky public storage.
pub(crate) fn for_reader(storage: &PublicStorage) -> PublicPaymentReader<'_> {
    PublicPaymentReader { storage }
}

/// Write-side Adapter for Paykit public data on the caller's Pubky homeserver.
#[derive(Clone, Copy)]
pub(crate) struct PublicPaymentStorage<'a> {
    session: &'a PubkySession,
}

impl PublicPaymentStorage<'_> {
    /// Writes or updates a public Payment Endpoint in the caller's Pubky storage.
    #[instrument(skip(self, payload), fields(identifier = %identifier))]
    pub(crate) async fn set_payment_endpoint(
        &self,
        identifier: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
    ) -> Result<()> {
        let path = PublicPaymentEndpointPath::local(identifier);
        debug!(path = %path.as_path(), "writing payment endpoint to Pubky storage");
        self.session
            .storage()
            .put(
                path.as_path().as_str().to_string(),
                payload.as_str().to_string(),
            )
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
    #[instrument(skip(self), fields(identifier = %identifier))]
    pub(crate) async fn remove_payment_endpoint(
        &self,
        identifier: &PaymentEndpointIdentifier,
    ) -> Result<()> {
        let path = PublicPaymentEndpointPath::local(identifier);
        debug!(path = %path.as_path(), "deleting payment endpoint from Pubky storage");
        self.session
            .storage()
            .delete(path.as_path().as_str().to_string())
            .await
            .map_err(|err| {
                error!(error = %err, "failed to delete payment endpoint");
                PaykitError::Transport {
                    context: "delete endpoint".into(),
                    source: err.into(),
                }
            })?;
        debug!("payment endpoint removed successfully");
        Ok(())
    }

    /// Stores an encrypted Receipt payload at its canonical Pubky Routing path.
    #[instrument(skip(self, encrypted), fields(reference = %reference))]
    pub(crate) async fn store_encrypted_receipt(
        &self,
        reference: &PaymentReference,
        encrypted: String,
    ) -> Result<String> {
        let path = ReceiptPayloadPath::local(reference);
        let location = path.as_path().as_str().to_string();
        debug!(path = %location, "writing encrypted receipt to Pubky storage");
        self.session
            .storage()
            .put(location.clone(), encrypted)
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to store encrypted receipt at {location}"),
                source: err.into(),
            })?;
        Ok(location)
    }
}

/// Read-side Adapter for Paykit public data on Pubky homeservers.
#[derive(Clone, Copy)]
pub(crate) struct PublicPaymentReader<'a> {
    storage: &'a PublicStorage,
}

impl PublicPaymentReader<'_> {
    /// Fetches the payee's public Payment List from Pubky storage.
    ///
    /// This first lists Payment Endpoint entries and then fetches each one
    /// individually. Pubky storage does not provide an atomic directory snapshot for
    /// this operation, so the returned [`PaymentList`] is best-effort if the payee
    /// mutates entries concurrently.
    #[instrument(skip(self), fields(payee = %payee))]
    pub(crate) async fn get_payment_list(&self, payee: &PublicKey) -> Result<PaymentList> {
        let addr = PublicPaymentListPath::addressed(payee);
        debug!(addr = %addr, "listing Payment Endpoint entries");
        let entries = self.list_entries(addr, "list payment endpoints").await?;

        let mut map = HashMap::new();
        for resource in entries {
            if resource.path.as_str().ends_with('/') {
                trace!(path = %resource.path, "skipping directory entry");
                continue;
            }

            let payment_endpoint_identifier =
                PublicPaymentEndpointPath::identifier_from_resource_path(resource.path.as_str())?;
            let label = format!("fetch endpoint {payment_endpoint_identifier}");
            if let Some(payload) = self.fetch_text(resource.to_string(), &label).await? {
                debug!(identifier = %payment_endpoint_identifier, "fetched payment endpoint payload");
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
    #[instrument(skip(self), fields(payee = %payee, identifier = %identifier))]
    pub(crate) async fn get_payment_endpoint(
        &self,
        payee: &PublicKey,
        identifier: &PaymentEndpointIdentifier,
    ) -> Result<Option<PaymentEndpointPayload>> {
        let addr = PublicPaymentEndpointPath::addressed(payee, identifier);
        debug!(addr = %addr, "fetching individual payment endpoint");
        match self.fetch_text(addr, "fetch endpoint").await? {
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

    #[instrument(skip(self), fields(addr = %addr, label = %label))]
    async fn fetch_text(&self, addr: String, label: &str) -> Result<Option<String>> {
        trace!("fetching text resource");
        match self.storage.get(&addr).await {
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
        let builder = match self.storage.list(&addr) {
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

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}
