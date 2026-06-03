use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{identity::IdentityState, PaykitSdkError, Result};

/// Durable storage boundary for Paykit SDK.
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Load the current identity state.
    async fn load_identity_state(&self) -> Result<Option<IdentityState>>;

    /// Save the current identity state atomically.
    async fn save_identity_state(&self, state: IdentityState) -> Result<()>;
}

/// In-memory storage state used for tests and examples.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageState {
    /// Current identity state.
    pub identity_state: Option<IdentityState>,
}

/// In-memory SDK storage implementation.
#[derive(Clone, Debug, Default)]
pub struct InMemoryStorage {
    state: Arc<Mutex<StorageState>>,
}

impl InMemoryStorage {
    /// Create empty in-memory storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a copy of the current storage state.
    pub fn snapshot(&self) -> Result<StorageState> {
        Ok(self
            .state
            .lock()
            .map_err(|err| PaykitSdkError::Storage {
                context: "in-memory storage lock poisoned".into(),
                source: Some(anyhow::anyhow!(err.to_string())),
            })?
            .clone())
    }
}

#[async_trait]
impl StorageAdapter for InMemoryStorage {
    async fn load_identity_state(&self) -> Result<Option<IdentityState>> {
        Ok(self.snapshot()?.identity_state)
    }

    async fn save_identity_state(&self, state: IdentityState) -> Result<()> {
        self.state
            .lock()
            .map_err(|err| PaykitSdkError::Storage {
                context: "in-memory storage lock poisoned".into(),
                source: Some(anyhow::anyhow!(err.to_string())),
            })?
            .identity_state = Some(state);
        Ok(())
    }
}
