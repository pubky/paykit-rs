use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use paykit_sdk::{
    PaykitSdkError, PubkyAuthDetails, PubkyAuthRequest, PubkyAuthRequestKind,
    PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess,
    PubkySessionBootstrap, PubkySessionBootstrapResult, PubkySessionProvider,
};
use pubky::{Pubky, PubkyHttpClient, PubkySession};
use tokio::sync::Mutex as AsyncMutex;
use zeroize::Zeroizing;

use crate::config::{default_pubky_client_config, FfiPubkyClientConfig};
use crate::errors::{ffi_error_to_sdk, identity_error, validation_error, PaykitFfiError};
use crate::secrets::FfiPubkyLocalSecretKey;

pub(crate) fn parse_public_key(value: String) -> Result<PubkyPublicKey, PaykitFfiError> {
    PubkyPublicKey::from_raw_or_app_key(value).map_err(Into::into)
}

pub(crate) fn app_public_key(value: &PubkyPublicKey) -> String {
    value.to_app_key()
}

pub(crate) fn raw_public_key(value: &PubkyPublicKey) -> String {
    value.as_str().to_owned()
}

/// Pubky capability state for one app-owned Paykit runtime.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPubkyIdentityCapability {
    /// No Pubky identity is initialized, or explicit sign-out completed.
    SignedOut,
    /// Public Pubky operations may work, but private links cannot be established.
    PublicOnly,
    /// Public operations and Encrypted Links can work.
    PrivateLinkCapable,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Kind of Pubky auth request represented by a deep link.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPubkyAuthRequestKind {
    /// Sign in to an existing Pubky account.
    SignIn,
    /// Sign up on a Pubky homeserver.
    SignUp,
    /// Export a secret from a signer.
    SecretExport,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Live Pubky access material supplied by platform session storage.
#[derive(uniffi::Object)]
pub struct FfiPubkySessionAccess {
    pub(crate) session_secret: String,
    pub(crate) local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
    pub(crate) live_access: Option<PubkySessionAccess>,
}

impl fmt::Debug for FfiPubkySessionAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPubkySessionAccess")
            .field("session_secret", &"<redacted>")
            .field(
                "local_secret_key",
                &self
                    .local_secret_key
                    .as_ref()
                    .map(|key| format!("<redacted:{} bytes>", key.bytes.len())),
            )
            .field("live_access", &self.live_access.as_ref().map(|_| "<live>"))
            .finish()
    }
}

#[uniffi::export]
impl FfiPubkySessionAccess {
    /// Create session access material from platform secure storage.
    #[uniffi::constructor]
    pub fn new(
        session_secret: String,
        local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
    ) -> Self {
        Self {
            session_secret,
            local_secret_key,
            live_access: None,
        }
    }

    /// Export the Pubky session bearer secret for platform secure storage.
    pub fn export_session_secret(&self) -> String {
        self.session_secret.clone()
    }

    /// Export the local Pubky secret key, when available.
    pub fn export_local_secret_key(&self) -> Option<Arc<FfiPubkyLocalSecretKey>> {
        self.local_secret_key.clone()
    }
}

/// Result of creating or importing a Pubky session.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPubkySessionBootstrapResult {
    /// Session access material to persist in platform session storage.
    pub session_access: Arc<FfiPubkySessionAccess>,
    /// Local Pubky public key.
    pub public_key: String,
    /// Capability implied by the session and optional local secret key.
    pub capability: FfiPubkyIdentityCapability,
}

/// Public details parsed from a Pubky auth deep link.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyAuthDetails {
    /// Auth request kind.
    pub kind: FfiPubkyAuthRequestKind,
    /// Requested capabilities as canonical Pubky capability text.
    pub capabilities: Option<String>,
    /// Relay URL used by the auth flow.
    pub relay_url: Option<String>,
    /// Homeserver requested by a signup flow.
    pub homeserver_public_key: Option<String>,
}

/// Parsed Pubky resource with a normalized owner and path.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyResourceRef {
    /// Resource owner public key.
    pub public_key: String,
    /// Absolute resource path.
    pub path: String,
    /// Transport URL resolved by the Pubky client.
    pub transport_url: String,
}

/// Platform-owned Pubky session provider.
#[uniffi::export(with_foreign)]
pub trait FfiSdkPubkySessionProvider: Send + Sync {
    /// Load current live Pubky session access, when available.
    fn load_session_access(&self) -> Result<Option<Arc<FfiPubkySessionAccess>>, PaykitFfiError>;

    /// Report whether unauthenticated public Pubky storage can be used.
    fn public_storage_available(&self) -> Result<bool, PaykitFfiError>;

    /// Clear platform session access during explicit SDK sign-out.
    fn clear_session_access(&self) -> Result<(), PaykitFfiError>;
}

#[derive(Clone)]
pub(crate) struct FfiSdkPubkySessionProviderAdapter {
    pub(crate) provider: Arc<dyn FfiSdkPubkySessionProvider>,
    pub(crate) pubky: Pubky,
}

#[async_trait]
impl PubkySessionProvider for FfiSdkPubkySessionProviderAdapter {
    async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
        let Some(access) = self
            .provider
            .load_session_access()
            .map_err(|err| ffi_error_to_sdk(err, "load Pubky session access"))?
        else {
            return Ok(None);
        };

        let local_secret_key = access
            .local_secret_key
            .clone()
            .map(|key| local_secret_from_bytes(key.export_bytes()))
            .transpose()
            .map_err(|err| ffi_error_to_sdk(err, "load local Pubky secret key"))?;

        if let Some(live_access) = &access.live_access {
            let mut live_access = live_access.clone();
            live_access.local_secret_key = local_secret_key;
            return Ok(Some(live_access));
        }

        let session =
            PubkySession::import_secret(&access.session_secret, Some(self.pubky.client().clone()))
                .await
                .map_err(|err| PaykitSdkError::Identity {
                    context: "import Pubky session from platform provider".into(),
                    source: Some(err.into()),
                })?;

        Ok(Some(PubkySessionAccess {
            session,
            outbox_client: self.pubky.clone(),
            local_secret_key,
        }))
    }

    async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
        let available = self
            .provider
            .public_storage_available()
            .map_err(|err| ffi_error_to_sdk(err, "load public Pubky storage"))?;
        Ok(available.then(|| self.pubky.public_storage()))
    }

    async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
        self.provider
            .clear_session_access()
            .map_err(|err| ffi_error_to_sdk(err, "clear Pubky session access"))
    }
}

/// Pubky session bootstrap helper.
#[derive(uniffi::Object)]
pub struct FfiPubkySessionBootstrap {
    inner: PubkySessionBootstrap,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPubkySessionBootstrap {
    /// Create a Pubky session bootstrap helper.
    #[uniffi::constructor]
    pub fn new() -> Result<Self, PaykitFfiError> {
        Self::with_pubky_client_config(default_pubky_client_config())
    }

    /// Create a Pubky session bootstrap helper with explicit Pubky client configuration.
    #[uniffi::constructor]
    pub fn with_pubky_client_config(
        pubky_client: FfiPubkyClientConfig,
    ) -> Result<Self, PaykitFfiError> {
        Ok(Self {
            inner: PubkySessionBootstrap::with_pubky(pubky_from_config(&pubky_client)?),
        })
    }

    /// Sign up on a homeserver and return session access material.
    pub async fn sign_up(
        &self,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
        homeserver_public_key: String,
        signup_code: Option<String>,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let secret = local_secret_from_bytes(local_secret_key.export_bytes())?;
        let homeserver = parse_public_key(homeserver_public_key)?;
        let result = self
            .inner
            .sign_up(&secret, &homeserver, signup_code.as_deref())
            .await?;
        Ok(bootstrap_result_to_ffi(result, Some(secret)))
    }

    /// Sign in with a local Pubky secret key and return session access material.
    pub async fn sign_in(
        &self,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let secret = local_secret_from_bytes(local_secret_key.export_bytes())?;
        let result = self.inner.sign_in(&secret).await?;
        Ok(bootstrap_result_to_ffi(result, Some(secret)))
    }

    /// Import an exported Pubky session secret.
    pub async fn import_session(
        &self,
        session_secret: String,
        local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
        required_capabilities: String,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let secret = local_secret_key
            .map(|key| local_secret_from_bytes(key.export_bytes()))
            .transpose()?;
        let result = self
            .inner
            .import_session(&session_secret, secret.clone(), &required_capabilities)
            .await?;
        Ok(bootstrap_result_to_ffi(result, secret))
    }

    /// Start a sign-in auth flow for an external signer.
    pub fn start_sign_in_auth(
        &self,
        capabilities: String,
    ) -> Result<Arc<FfiPubkyAuthRequest>, PaykitFfiError> {
        Ok(Arc::new(FfiPubkyAuthRequest {
            inner: AsyncMutex::new(Some(self.inner.start_sign_in_auth(&capabilities)?)),
        }))
    }

    /// Start a signup auth flow for an external signer.
    pub fn start_sign_up_auth(
        &self,
        capabilities: String,
        homeserver_public_key: String,
        signup_token: Option<String>,
    ) -> Result<Arc<FfiPubkyAuthRequest>, PaykitFfiError> {
        let homeserver = parse_public_key(homeserver_public_key)?;
        Ok(Arc::new(FfiPubkyAuthRequest {
            inner: AsyncMutex::new(Some(self.inner.start_sign_up_auth(
                &capabilities,
                &homeserver,
                signup_token,
            )?)),
        }))
    }

    /// Resume a short-lived auth flow from its authorization URL.
    pub fn resume_auth(
        &self,
        authorization_url: String,
        expected_capabilities: String,
    ) -> Result<Arc<FfiPubkyAuthRequest>, PaykitFfiError> {
        Ok(Arc::new(FfiPubkyAuthRequest {
            inner: AsyncMutex::new(Some(
                self.inner
                    .resume_auth(&authorization_url, &expected_capabilities)?,
            )),
        }))
    }

    /// Approve a Pubky auth URL with this local secret key.
    pub async fn approve_auth(
        &self,
        auth_url: String,
        expected_capabilities: String,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
    ) -> Result<(), PaykitFfiError> {
        let secret = local_secret_from_bytes(local_secret_key.export_bytes())?;
        self.inner
            .approve_auth(&auth_url, &expected_capabilities, &secret)
            .await
            .map_err(Into::into)
    }
}

/// Pending Pubky auth request.
#[derive(uniffi::Object)]
pub struct FfiPubkyAuthRequest {
    inner: AsyncMutex<Option<PubkyAuthRequest>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPubkyAuthRequest {
    /// Return the auth URL to show as a deeplink or QR code.
    pub async fn authorization_url(&self) -> Result<String, PaykitFfiError> {
        let guard = self.inner.lock().await;
        guard
            .as_ref()
            .map(|request| request.authorization_url().to_string())
            .ok_or_else(|| validation_error("Pubky auth request already completed"))
    }

    /// Wait for auth approval and validate the resulting session capabilities.
    pub async fn complete(
        &self,
        local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
        required_capabilities: String,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let secret = local_secret_key
            .map(|key| local_secret_from_bytes(key.export_bytes()))
            .transpose()?;
        let request = self
            .inner
            .lock()
            .await
            .take()
            .ok_or_else(|| validation_error("Pubky auth request already completed"))?;
        let result = request
            .complete(secret.clone(), &required_capabilities)
            .await?;
        Ok(bootstrap_result_to_ffi(result, secret))
    }
}

/// Derive a local Pubky secret key from a 64-byte BIP39 seed.
#[uniffi::export]
pub fn pubky_secret_key_from_bip39_seed(
    seed: Vec<u8>,
) -> Result<Arc<FfiPubkyLocalSecretKey>, PaykitFfiError> {
    let seed = Zeroizing::new(seed);
    let key = PubkyLocalSecretKey::from_bip39_seed(&seed)?;
    Ok(secret_to_ffi(&key))
}

/// Derive a local Pubky secret key from a BIP39 English mnemonic phrase.
#[uniffi::export]
pub fn pubky_secret_key_from_bip39_mnemonic(
    mnemonic_phrase: String,
) -> Result<Arc<FfiPubkyLocalSecretKey>, PaykitFfiError> {
    let mnemonic_phrase = Zeroizing::new(mnemonic_phrase);
    let key = PubkyLocalSecretKey::from_bip39_mnemonic(&mnemonic_phrase)?;
    Ok(secret_to_ffi(&key))
}

/// Return the Pubky public key for a local secret key.
#[uniffi::export]
pub fn pubky_public_key_from_secret(
    local_secret_key: Arc<FfiPubkyLocalSecretKey>,
) -> Result<String, PaykitFfiError> {
    Ok(local_secret_from_bytes(local_secret_key.export_bytes())?
        .public_key()
        .to_app_key())
}

/// Normalize raw z32 or `pubky...` public-key text to app-key form.
#[uniffi::export]
pub fn normalize_pubky_public_key(value: String) -> Result<String, PaykitFfiError> {
    parse_public_key(value).map(|key| key.to_app_key())
}

/// Normalize raw z32 or `pubky...` public-key text to raw z32 form.
#[uniffi::export]
pub fn raw_pubky_public_key(value: String) -> Result<String, PaykitFfiError> {
    parse_public_key(value).map(|key| raw_public_key(&key))
}

/// Return a shortened `pubky...` public key for diagnostics.
#[uniffi::export]
pub fn redacted_pubky_public_key(value: String) -> Result<String, PaykitFfiError> {
    parse_public_key(value).map(|key| key.redacted_app_key())
}

/// Parse an auth deep link into public request details.
#[uniffi::export]
pub fn parse_pubky_auth_url(auth_url: String) -> Result<FfiPubkyAuthDetails, PaykitFfiError> {
    paykit_sdk::parse_pubky_auth_url(&auth_url)
        .map(Into::into)
        .map_err(Into::into)
}

/// Resolve a Pubky URI into the transport URL used by Pubky storage.
#[uniffi::export]
pub fn resolve_pubky_url(uri: String) -> Result<String, PaykitFfiError> {
    paykit_sdk::resolve_pubky_url(&uri).map_err(Into::into)
}

/// Parse a `pubky://<public-key>/<path>` resource into stable parts.
#[uniffi::export]
pub fn parse_pubky_resource(uri: String) -> Result<FfiPubkyResourceRef, PaykitFfiError> {
    paykit_sdk::parse_pubky_resource(&uri)
        .map(Into::into)
        .map_err(Into::into)
}

pub(crate) fn pubky_from_config(config: &FfiPubkyClientConfig) -> Result<Pubky, PaykitFfiError> {
    if config.request_timeout_secs == 0 {
        return Err(validation_error(
            "pubky request timeout must be greater than zero",
        ));
    }

    let mut builder = PubkyHttpClient::builder();
    builder.request_timeout(Duration::from_secs(config.request_timeout_secs));
    builder
        .build()
        .map(Pubky::with_client)
        .map_err(|_err| identity_error("pubky_client", "create Pubky client failed"))
}

pub(crate) fn local_secret_from_bytes(
    bytes: Vec<u8>,
) -> Result<PubkyLocalSecretKey, PaykitFfiError> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        validation_error(format!(
            "Pubky local secret key must be 32 bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(PubkyLocalSecretKey::new(bytes))
}

pub(crate) fn secret_to_ffi(secret: &PubkyLocalSecretKey) -> Arc<FfiPubkyLocalSecretKey> {
    Arc::new(FfiPubkyLocalSecretKey::new(secret.as_bytes().to_vec()))
}

fn bootstrap_result_to_ffi(
    result: PubkySessionBootstrapResult,
    local_secret_key: Option<PubkyLocalSecretKey>,
) -> FfiPubkySessionBootstrapResult {
    let session_secret = result.export_session_secret().into_inner();
    let live_access = result.access.clone();
    FfiPubkySessionBootstrapResult {
        session_access: Arc::new(FfiPubkySessionAccess {
            session_secret,
            local_secret_key: local_secret_key.as_ref().map(secret_to_ffi),
            live_access: Some(live_access),
        }),
        public_key: app_public_key(&result.public_key),
        capability: result.capability.into(),
    }
}

impl From<PubkyIdentityCapability> for FfiPubkyIdentityCapability {
    fn from(value: PubkyIdentityCapability) -> Self {
        match value {
            PubkyIdentityCapability::SignedOut => Self::SignedOut,
            PubkyIdentityCapability::PublicOnly => Self::PublicOnly,
            PubkyIdentityCapability::PrivateLinkCapable => Self::PrivateLinkCapable,
            _ => Self::Unknown,
        }
    }
}

impl From<PubkyAuthRequestKind> for FfiPubkyAuthRequestKind {
    fn from(value: PubkyAuthRequestKind) -> Self {
        match value {
            PubkyAuthRequestKind::SignIn => Self::SignIn,
            PubkyAuthRequestKind::SignUp => Self::SignUp,
            PubkyAuthRequestKind::SecretExport => Self::SecretExport,
            _ => Self::Unknown,
        }
    }
}

impl From<PubkyAuthDetails> for FfiPubkyAuthDetails {
    fn from(value: PubkyAuthDetails) -> Self {
        Self {
            kind: value.kind.into(),
            capabilities: value.capabilities,
            relay_url: value.relay_url,
            homeserver_public_key: value.homeserver_public_key.map(|key| key.to_app_key()),
        }
    }
}

impl From<paykit_sdk::PubkyResourceRef> for FfiPubkyResourceRef {
    fn from(value: paykit_sdk::PubkyResourceRef) -> Self {
        Self {
            public_key: value.public_key.to_app_key(),
            path: value.path,
            transport_url: value.transport_url,
        }
    }
}
