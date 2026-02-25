//! Authenticated Pubky adapter that satisfies [`crate::AuthenticatedTransport`].

use async_trait::async_trait;
use pubky::PubkySession;
use tracing::{debug, error, instrument};

use super::PAYKIT_PATH_PREFIX;
use crate::transport::policy::{execute_with_policy, TransportPolicy};
use crate::transport::traits::AuthenticatedTransport;
use crate::{EndpointData, MethodId, PaykitError, Result};

/// Adapter around `pubky::PubkySession` implementing `AuthenticatedTransport`.
///
/// Every instance carries a [`TransportPolicy`] that governs timeout and retry
/// behaviour. The default policy (30 s timeout, 3 retries with exponential
/// backoff) is applied automatically — use [`with_policy`](Self::with_policy)
/// only when you need non-default settings.
#[derive(Clone)]
pub struct PubkyAuthenticatedTransport {
    session: PubkySession,
    policy: TransportPolicy,
}

impl PubkyAuthenticatedTransport {
    /// Create a new adapter from an existing session.
    ///
    /// Uses [`TransportPolicy::default()`] (30 s timeout, 3 retries).
    pub fn new(session: PubkySession) -> Self {
        Self {
            session,
            policy: TransportPolicy::default(),
        }
    }

    /// Override the transport policy.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::time::Duration;
    /// # use paykit_lib::{PubkyAuthenticatedTransport, TransportPolicy};
    /// # fn demo(session: pubky::PubkySession) {
    /// let transport = PubkyAuthenticatedTransport::new(session)
    ///     .with_policy(TransportPolicy::builder()
    ///         .timeout(Duration::from_secs(5))
    ///         .max_retries(1)
    ///         .build());
    /// # }
    /// ```
    pub fn with_policy(mut self, policy: TransportPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Access the wrapped session for advanced payers/payees.
    pub fn session(&self) -> &PubkySession {
        &self.session
    }

    /// Access the current transport policy.
    pub fn policy(&self) -> &TransportPolicy {
        &self.policy
    }
}

impl From<PubkySession> for PubkyAuthenticatedTransport {
    fn from(session: PubkySession) -> Self {
        Self::new(session)
    }
}

#[async_trait]
impl AuthenticatedTransport for PubkyAuthenticatedTransport {
    #[instrument(skip(self, data), fields(method = %method))]
    async fn upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()> {
        let path = format!("{PAYKIT_PATH_PREFIX}{}", method.as_str());
        debug!(path = %path, "writing payment endpoint to storage");

        execute_with_policy(&self.policy, "upsert_payment_endpoint", || {
            let path = path.clone();
            let payload = data.as_str().to_string();
            async move {
                self.session
                    .storage()
                    .put(path, payload)
                    .await
                    .map_err(|err| {
                        error!(error = %err, "failed to put payment endpoint");
                        PaykitError::Transport {
                            context: "put endpoint".into(),
                            source: err.into(),
                        }
                    })
            }
        })
        .await?;

        debug!("payment endpoint stored successfully");
        Ok(())
    }

    #[instrument(skip(self), fields(method = %method))]
    async fn remove_payment_endpoint(&self, method: &MethodId) -> Result<()> {
        let path = format!("{PAYKIT_PATH_PREFIX}{}", method.as_str());
        debug!(path = %path, "deleting payment endpoint from storage");

        execute_with_policy(&self.policy, "remove_payment_endpoint", || {
            let path = path.clone();
            async move {
                self.session.storage().delete(path).await.map_err(|err| {
                    error!(error = %err, "failed to delete payment endpoint");
                    PaykitError::Transport {
                        context: "delete endpoint".into(),
                        source: err.into(),
                    }
                })
            }
        })
        .await?;

        debug!("payment endpoint removed successfully");
        Ok(())
    }
}
