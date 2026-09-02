use std::sync::{Arc, Mutex};

use paykit_sdk::{IdentityStatus, PaykitSdk, PubkySharedStateStorage, RestoreReport};

use crate::config::{default_pubky_client_config, FfiPaykitSdkConfig, FfiPubkyClientConfig};
use crate::errors::{validation_error, PaykitFfiError};
use crate::payment_adapter::{
    FfiNoopSdkPaymentAdapter, FfiSdkPaymentAdapter, FfiSdkPaymentAdapterAdapter,
};
use crate::secrets::FfiSdkBackupBlob;
use crate::session::{
    app_public_key, pubky_from_config, FfiPubkyIdentityCapability, FfiSdkPubkySessionProvider,
    FfiSdkPubkySessionProviderAdapter,
};
use crate::storage::{
    decode_backup_state, encode_backup_state, FfiSdkStateBlobStore, FfiSdkStorage,
    FfiSdkStorageAdapter,
};

/// Current identity status returned to apps.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiIdentityStatus {
    /// Last initialized public key, when known.
    pub public_key: Option<String>,
    /// Current Pubky capability.
    pub capability: FfiPubkyIdentityCapability,
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
    /// Number of restored Contact Records.
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
    PaykitSdk<FfiSdkStorageAdapter, FfiSdkPubkySessionProviderAdapter, FfiSdkPaymentAdapterAdapter>;

/// Stateful Paykit SDK runtime handle.
#[derive(uniffi::Object)]
pub struct FfiPaykitSdk {
    pub(crate) runtime: FfiSdkRuntime,
    storage: FfiSdkStorageAdapter,
}

#[uniffi::export(async_runtime = "tokio")]
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
        let session_provider = ffi_session_provider(session_provider, pubky_client)?;
        let storage = FfiSdkStorageAdapter::Callback(FfiSdkStorage {
            store: state_store,
            transaction_lock: Arc::new(Mutex::new(())),
        });
        build_sdk(storage, session_provider, payment_adapter, config)
    }

    /// Create an SDK runtime with encrypted identity-wide state stored in Pubky.
    ///
    /// Every operation requires active session access with the matching local
    /// identity secret and `required_session_capabilities()`. Homeserver ETag
    /// preconditions reject stale writes from independent runtimes; callers may
    /// retry the complete SDK operation after a shared-state conflict.
    #[uniffi::constructor]
    pub fn with_pubky_shared_state(
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        config: FfiPaykitSdkConfig,
    ) -> Result<Self, PaykitFfiError> {
        Self::with_pubky_shared_state_and_client_config(
            session_provider,
            config,
            default_pubky_client_config(),
        )
    }

    /// Create a Pubky shared-state runtime with explicit client configuration.
    ///
    /// Requires active session access with current Paykit identity key material
    /// and `required_session_capabilities()`. Homeserver ETag preconditions
    /// reject stale writes from independent runtimes.
    #[uniffi::constructor]
    pub fn with_pubky_shared_state_and_client_config(
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        config: FfiPaykitSdkConfig,
        pubky_client: FfiPubkyClientConfig,
    ) -> Result<Self, PaykitFfiError> {
        Self::with_payment_adapter_and_pubky_shared_state_and_client_config(
            session_provider,
            Arc::new(FfiNoopSdkPaymentAdapter),
            config,
            pubky_client,
        )
    }

    /// Create a Pubky shared-state runtime with payment adapter callbacks.
    ///
    /// Requires active session access with current Paykit identity key material
    /// and `required_session_capabilities()`. Homeserver ETag preconditions
    /// reject stale writes from independent runtimes.
    #[uniffi::constructor]
    pub fn with_payment_adapter_and_pubky_shared_state(
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        payment_adapter: Arc<dyn FfiSdkPaymentAdapter>,
        config: FfiPaykitSdkConfig,
    ) -> Result<Self, PaykitFfiError> {
        Self::with_payment_adapter_and_pubky_shared_state_and_client_config(
            session_provider,
            payment_adapter,
            config,
            default_pubky_client_config(),
        )
    }

    /// Create a Pubky shared-state runtime with payment and client configuration.
    ///
    /// Requires active session access with current Paykit identity key material
    /// and `required_session_capabilities()`. Homeserver ETag preconditions
    /// reject stale writes from independent runtimes.
    #[uniffi::constructor]
    pub fn with_payment_adapter_and_pubky_shared_state_and_client_config(
        session_provider: Arc<dyn FfiSdkPubkySessionProvider>,
        payment_adapter: Arc<dyn FfiSdkPaymentAdapter>,
        config: FfiPaykitSdkConfig,
        pubky_client: FfiPubkyClientConfig,
    ) -> Result<Self, PaykitFfiError> {
        let session_provider = ffi_session_provider(session_provider, pubky_client)?;
        let storage = FfiSdkStorageAdapter::PubkyShared(PubkySharedStateStorage::new(
            session_provider.clone(),
        ));
        build_sdk(storage, session_provider, payment_adapter, config)
    }

    /// Return this runtime's configuration.
    pub fn config(&self) -> FfiPaykitSdkConfig {
        self.runtime.config().clone().into()
    }

    /// Return the latest observed SDK state revision, when one exists.
    pub fn state_revision(&self) -> Result<Option<String>, PaykitFfiError> {
        self.storage.state_revision()
    }

    /// Initialize durable SDK identity state.
    pub async fn initialize(&self) -> Result<FfiIdentityStatus, PaykitFfiError> {
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

    /// Revoke the current Pubky grant and clear local session access.
    ///
    /// Identity-wide Paykit state remains available to other applications.
    pub async fn sign_out(&self) -> Result<FfiIdentityStatus, PaykitFfiError> {
        self.runtime
            .sign_out()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Clear local session access and SDK identity state without revoking the grant.
    ///
    /// Use this only when remote revocation cannot be reached and the app
    /// intentionally accepts that persisted copies of the grant remain valid.
    pub async fn forget_session_access(&self) -> Result<FfiIdentityStatus, PaykitFfiError> {
        self.runtime
            .forget_session_access()
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

    /// Export SDK-managed backup state as a hex string.
    pub async fn export_backup_string(&self) -> Result<String, PaykitFfiError> {
        self.export_backup_state()
            .await
            .map(|backup| hex::encode(backup.export_bytes()))
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

    /// Restore SDK-managed backup state from a hex string.
    pub async fn restore_backup_string(
        &self,
        backup: String,
    ) -> Result<FfiRestoreReport, PaykitFfiError> {
        let bytes = hex::decode(backup.trim())
            .map_err(|err| validation_error(format!("invalid SDK backup string: {err}")))?;
        self.restore_backup_state(Arc::new(FfiSdkBackupBlob::new(bytes)))
            .await
    }
}

fn ffi_session_provider(
    provider: Arc<dyn FfiSdkPubkySessionProvider>,
    pubky_client: FfiPubkyClientConfig,
) -> Result<FfiSdkPubkySessionProviderAdapter, PaykitFfiError> {
    Ok(FfiSdkPubkySessionProviderAdapter::new(
        provider,
        pubky_from_config(&pubky_client)?,
        pubky_client,
    ))
}

fn build_sdk(
    storage: FfiSdkStorageAdapter,
    session_provider: FfiSdkPubkySessionProviderAdapter,
    payment_adapter: Arc<dyn FfiSdkPaymentAdapter>,
    config: FfiPaykitSdkConfig,
) -> Result<FfiPaykitSdk, PaykitFfiError> {
    let runtime = PaykitSdk::new(
        storage.clone(),
        session_provider,
        FfiSdkPaymentAdapterAdapter {
            adapter: payment_adapter,
        },
        config.try_into()?,
    );
    Ok(FfiPaykitSdk { runtime, storage })
}

impl From<IdentityStatus> for FfiIdentityStatus {
    fn from(value: IdentityStatus) -> Self {
        Self {
            public_key: value.public_key.map(|key| app_public_key(&key)),
            capability: value.capability.into(),
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
                .map(|key| app_public_key(&key))
                .collect(),
        }
    }
}
