use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    IdentityStatus, PaykitSdkConfig, PaymentAdapter, PubkySessionProvider, Result, StorageAdapter,
};

/// Clock abstraction used by SDK workflows and tests.
pub trait Clock: Clone + Send + Sync + 'static {
    /// Return the current UTC time.
    fn now(&self) -> DateTime<Utc>;
}

/// System UTC clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Initialization report returned after SDK startup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationReport {
    /// Last persisted identity status.
    pub identity: IdentityStatus,
    /// Whether the provider returned live Pubky session access during startup.
    pub live_session_available: bool,
}

/// Stateful Paykit SDK runtime for one local Pubky identity.
pub struct PaykitSdk<S, K, P, C = SystemClock> {
    storage: S,
    pubky: K,
    payment: P,
    config: PaykitSdkConfig,
    clock: C,
}

impl<S, K, P> PaykitSdk<S, K, P, SystemClock>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
{
    /// Create an SDK runtime with the system clock.
    pub fn new(storage: S, pubky: K, payment: P, config: PaykitSdkConfig) -> Result<Self> {
        Self::try_with_clock(storage, pubky, payment, config, SystemClock)
    }

    /// Fallible alias for [`Self::new`].
    pub fn try_new(storage: S, pubky: K, payment: P, config: PaykitSdkConfig) -> Result<Self> {
        Self::new(storage, pubky, payment, config)
    }
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Create an SDK runtime with an explicit clock.
    pub fn try_with_clock(
        storage: S,
        pubky: K,
        payment: P,
        config: PaykitSdkConfig,
        clock: C,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            storage,
            pubky,
            payment,
            config,
            clock,
        })
    }

    /// Access the storage adapter used by this SDK instance.
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Access the Pubky session provider used by this SDK instance.
    pub fn pubky_session_provider(&self) -> &K {
        &self.pubky
    }

    /// Access the payment adapter used by this SDK instance.
    pub fn payment_adapter(&self) -> &P {
        &self.payment
    }

    /// Access the SDK configuration.
    pub fn config(&self) -> &PaykitSdkConfig {
        &self.config
    }

    /// Access the SDK clock.
    pub fn clock(&self) -> &C {
        &self.clock
    }
}
