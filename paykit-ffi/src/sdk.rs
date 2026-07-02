use std::sync::Arc;

use paykit_sdk::{IdentityStatus, InitializationReport, PaykitSdk, RestoreReport};

use crate::config::{default_pubky_client_config, FfiPaykitSdkConfig, FfiPubkyClientConfig};
use crate::errors::PaykitFfiError;
use crate::payment_adapter::{
    FfiNoopSdkPaymentAdapter, FfiSdkPaymentAdapter, FfiSdkPaymentAdapterAdapter,
};
use crate::secrets::FfiSdkBackupBlob;
use crate::session::{
    pubky_from_config, FfiPubkyIdentityCapability, FfiSdkPubkySessionProvider,
    FfiSdkPubkySessionProviderAdapter,
};
use crate::storage::{
    decode_backup_state, encode_backup_state, FfiSdkStateBlobStore, FfiSdkStorage,
};

/// Current identity status returned to apps.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiIdentityStatus {
    /// Current local public key, when signed in.
    pub public_key: Option<String>,
    /// Current Pubky capability.
    pub capability: FfiPubkyIdentityCapability,
    /// Whether live Pubky session access is available for this identity.
    pub live_session_available: bool,
    /// Whether private Paykit workflows can run with the live session.
    pub private_link_capable: bool,
}

/// Initialization report returned after SDK startup.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiInitializationReport {
    /// Last persisted identity status.
    pub identity: FfiIdentityStatus,
    /// Whether live Pubky session access was available during startup.
    pub live_session_available: bool,
}

/// Report returned after restoring SDK-managed backup state.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiRestoreReport {
    /// Restored backup schema version.
    pub version: u32,
    /// Whether identity state was restored.
    pub restored_identity: bool,
    /// Number of restored Linked Peer records.
    pub linked_peers: u64,
    /// Number of restored local contact records.
    pub contact_records: u64,
    /// Number of restored public Payment Endpoint records.
    pub public_endpoint_records: u64,
    /// Number of restored Payment Endpoint Reservation records.
    pub payment_endpoint_reservations: u64,
    /// Number of restored Encrypted Link state records.
    pub encrypted_link_states: u64,
    /// Number of restored outbound Private Application Message records.
    pub outbound_private_messages: u64,
    /// Number of restored private stream item records.
    pub private_stream_items: u64,
    /// Number of restored Event Message dedupe records.
    pub event_dedup_records: u64,
    /// Number of restored Receipt Access records.
    pub receipt_access_records: u64,
    /// Number of restored decrypted Receipt records.
    pub receipt_records: u64,
    /// Number of restored local receipt issuance records.
    pub receipt_issuance_records: u64,
    /// Counterparties restored as recovery-required.
    pub recovery_required_peers: Vec<String>,
}

pub(crate) type FfiSdkRuntime =
    PaykitSdk<FfiSdkStorage, FfiSdkPubkySessionProviderAdapter, FfiSdkPaymentAdapterAdapter>;

/// Stateful Paykit SDK runtime handle.
#[derive(uniffi::Object)]
pub struct FfiPaykitSdk {
    pub(crate) runtime: FfiSdkRuntime,
}

#[uniffi::export]
impl FfiPaykitSdk {
    /// Create an SDK runtime from platform storage/session callbacks.
    #[uniffi::constructor]
    pub fn new(
        state_store: Arc<dyn FfiSdkStateBlobStore>,
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        config: FfiPaykitSdkConfig,
    ) -> Result<Self, PaykitFfiError> {
        Self::with_pubky_client_config(
            state_store,
            session_provider,
            config,
            default_pubky_client_config(),
        )
    }

    /// Create an SDK runtime with explicit Pubky client configuration.
    #[uniffi::constructor]
    pub fn with_pubky_client_config(
        state_store: Arc<dyn FfiSdkStateBlobStore>,
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        config: FfiPaykitSdkConfig,
        pubky_client: FfiPubkyClientConfig,
    ) -> Result<Self, PaykitFfiError> {
        Self::with_payment_adapter_and_pubky_client_config(
            state_store,
            session_provider,
            Arc::new(FfiNoopSdkPaymentAdapter),
            config,
            pubky_client,
        )
    }

    /// Create an SDK runtime with payment adapter callbacks.
    #[uniffi::constructor]
    pub fn with_payment_adapter(
        state_store: Arc<dyn FfiSdkStateBlobStore>,
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        payment_adapter: Arc<dyn FfiSdkPaymentAdapter>,
        config: FfiPaykitSdkConfig,
    ) -> Result<Self, PaykitFfiError> {
        Self::with_payment_adapter_and_pubky_client_config(
            state_store,
            session_provider,
            payment_adapter,
            config,
            default_pubky_client_config(),
        )
    }

    /// Create an SDK runtime with payment adapter callbacks and Pubky client configuration.
    #[uniffi::constructor]
    pub fn with_payment_adapter_and_pubky_client_config(
        state_store: Arc<dyn FfiSdkStateBlobStore>,
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        payment_adapter: Arc<dyn FfiSdkPaymentAdapter>,
        config: FfiPaykitSdkConfig,
        pubky_client: FfiPubkyClientConfig,
    ) -> Result<Self, PaykitFfiError> {
        let pubky = pubky_from_config(&pubky_client)?;
        let storage = FfiSdkStorage { store: state_store };
        let session_provider = FfiSdkPubkySessionProviderAdapter {
            provider: session_provider,
            pubky,
        };
        let payment_adapter = FfiSdkPaymentAdapterAdapter {
            adapter: payment_adapter,
        };
        let runtime = PaykitSdk::new(
            storage,
            session_provider,
            payment_adapter,
            config.try_into()?,
        )?;
        Ok(Self { runtime })
    }

    /// Return this runtime's configuration.
    pub fn config(&self) -> FfiPaykitSdkConfig {
        self.runtime.config().clone().into()
    }

    /// Initialize durable SDK identity state.
    pub async fn initialize(&self) -> Result<FfiInitializationReport, PaykitFfiError> {
        self.runtime
            .initialize()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Return current identity status, when initialized.
    pub async fn identity_status(&self) -> Result<Option<FfiIdentityStatus>, PaykitFfiError> {
        self.runtime
            .identity_status()
            .await
            .map(|status| status.map(Into::into))
            .map_err(Into::into)
    }

    /// Clear live Pubky session access and SDK-managed identity-scoped state.
    pub async fn sign_out(&self) -> Result<FfiIdentityStatus, PaykitFfiError> {
        self.runtime
            .sign_out()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Export SDK-managed backup state as an opaque blob.
    pub async fn export_backup_state(&self) -> Result<Arc<FfiSdkBackupBlob>, PaykitFfiError> {
        let backup = self.runtime.export_backup_state().await?;
        Ok(Arc::new(FfiSdkBackupBlob::new(encode_backup_state(
            &backup,
        )?)))
    }

    /// Restore SDK-managed backup state from an opaque blob.
    pub async fn restore_backup_state(
        &self,
        backup: Arc<FfiSdkBackupBlob>,
    ) -> Result<FfiRestoreReport, PaykitFfiError> {
        let backup = decode_backup_state(&backup.export_bytes())?;
        self.runtime
            .restore_backup_state(backup)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<IdentityStatus> for FfiIdentityStatus {
    fn from(value: IdentityStatus) -> Self {
        Self {
            public_key: value.public_key.map(|key| key.to_string()),
            capability: value.capability.into(),
            live_session_available: value.live_session_available,
            private_link_capable: value.private_link_capable,
        }
    }
}

impl From<InitializationReport> for FfiInitializationReport {
    fn from(value: InitializationReport) -> Self {
        Self {
            identity: value.identity.into(),
            live_session_available: value.live_session_available,
        }
    }
}

impl From<RestoreReport> for FfiRestoreReport {
    fn from(value: RestoreReport) -> Self {
        Self {
            version: value.version,
            restored_identity: value.restored_identity,
            linked_peers: value.linked_peers as u64,
            contact_records: value.contact_records as u64,
            public_endpoint_records: value.public_endpoint_records as u64,
            payment_endpoint_reservations: value.payment_endpoint_reservations as u64,
            encrypted_link_states: value.encrypted_link_states as u64,
            outbound_private_messages: value.outbound_private_messages as u64,
            private_stream_items: value.private_stream_items as u64,
            event_dedup_records: value.event_dedup_records as u64,
            receipt_access_records: value.receipt_access_records as u64,
            receipt_records: value.receipt_records as u64,
            receipt_issuance_records: value.receipt_issuance_records as u64,
            recovery_required_peers: value
                .recovery_required_peers
                .into_iter()
                .map(|key| key.to_string())
                .collect(),
        }
    }
}
