use std::sync::Arc;

use paykit_sdk::{PubkyGrantObservation, PubkyGrantObserver};

use crate::{session::parse_public_key, FfiPaykitSdk, PaykitFfiError};

/// Point-in-time original-grant evidence, never permission to mutate.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPubkyGrantObservation {
    /// The server accepted the captured bearer and its exact original binding.
    Active,
    /// Status could not be established; this never means revoked or absent.
    Unknown,
}

/// Live-only original-grant observer. Contains a non-exportable secret bearer.
#[derive(uniffi::Object)]
pub struct FfiPubkyGrantObserver {
    inner: PubkyGrantObserver,
}

impl_redacted_debug!(FfiPubkyGrantObserver);

#[uniffi::export(async_runtime = "tokio")]
impl FfiPubkyGrantObserver {
    /// Original grant owner, in the app's public-key representation.
    pub fn public_key(&self) -> String {
        self.inner.public_key().to_app_key()
    }

    /// Original client identifier.
    pub fn client_id(&self) -> String {
        self.inner.client_id()
    }

    /// Original grant identifier, not a bearer or local-session generation.
    /// Restoring or refreshing the same grant can retain this ID while replacing
    /// its bearer. A newly issued replacement grant has a different ID.
    pub fn grant_id(&self) -> String {
        self.inner.grant_id()
    }

    /// Observe without a provider callback, session restore/refresh or storage.
    ///
    /// Retain this handle with the original owner/attempt, not a replacement
    /// session. A positive result is not a lease or a completed cleanup receipt.
    /// Recheck operation admission and require fresh confirmation before any
    /// subsequent mutation. Unknown results do not authorize remote retry.
    pub async fn observe(&self) -> FfiPubkyGrantObservation {
        match self.inner.observe().await {
            PubkyGrantObservation::Active => FfiPubkyGrantObservation::Active,
            _ => FfiPubkyGrantObservation::Unknown,
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Capture the admitted owner's existing live grant without authenticating.
    ///
    /// Reads the platform session callback and an existing native cached/live
    /// handle. A missing or changed cache fails rather than restoring credentials.
    /// Call before the original operation while its owner/generation is admitted,
    /// then check admission again before retaining the returned observer. The
    /// observer cannot recover an old attempt after process death.
    pub async fn capture_grant_observer(
        &self,
        expected_public_key: String,
    ) -> Result<Arc<FfiPubkyGrantObserver>, PaykitFfiError> {
        let owner = parse_public_key(expected_public_key)?;
        let inner = self.session_provider.capture_grant_observer(&owner).await?;
        Ok(Arc::new(FfiPubkyGrantObserver { inner }))
    }
}
