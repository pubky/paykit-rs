use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use paykit_sdk::{
    PaykitReceiverPath, PaykitSdkError, PubkyAuthCompanionClaim,
    PubkyAuthCompanionClaimApprovalError, PubkyAuthDetails, PubkyAuthRequest, PubkyAuthRequestKind,
    PubkyAuthRequestState, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess,
    PubkySessionBootstrap, PubkySessionBootstrapResult, PubkySessionProvider,
    ReceiverNoiseSecretKey,
};
use pubky::{ClientId, Pubky, PubkyHttpClient};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use zeroize::{Zeroize, Zeroizing};

use crate::config::{default_pubky_client_config, FfiPubkyClientConfig};
use crate::errors::{ffi_error_to_sdk, identity_error, validation_error, PaykitFfiError};
use crate::secrets::{FfiPubkyLocalSecretKey, FfiReceiverNoiseSecretKey};

pub(crate) fn parse_public_key(value: String) -> Result<PubkyPublicKey, PaykitFfiError> {
    PubkyPublicKey::from_raw_or_app_key(value).map_err(Into::into)
}

pub(crate) fn parse_receiver_path(value: String) -> Result<PaykitReceiverPath, PaykitFfiError> {
    PaykitReceiverPath::new(value).map_err(|err| validation_error(err.to_string()))
}

pub(crate) fn app_public_key(value: &PubkyPublicKey) -> String {
    value.to_app_key()
}

pub(crate) fn raw_public_key(value: &PubkyPublicKey) -> String {
    value.as_str().to_owned()
}

/// Kind of Pubky auth request represented by a deep link.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPubkyAuthRequestKind {
    /// Sign in to an existing Pubky account.
    SignIn,
    /// Sign up on a Pubky homeserver.
    SignUp,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Live Pubky access material supplied by platform session storage.
#[derive(uniffi::Object)]
pub struct FfiPubkySessionAccess {
    pub(crate) client_id: String,
    pub(crate) session_secret: String,
    pub(crate) local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
    pub(crate) receiver_noise_secret_key: Arc<FfiReceiverNoiseSecretKey>,
    pub(crate) live_access: Option<PubkySessionAccess>,
}

impl Drop for FfiPubkySessionAccess {
    fn drop(&mut self) {
        self.session_secret.zeroize();
    }
}

impl fmt::Debug for FfiPubkySessionAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPubkySessionAccess")
            .field("client_id", &self.client_id)
            .field("session_secret", &"<redacted>")
            .field(
                "local_secret_key",
                &self
                    .local_secret_key
                    .as_ref()
                    .map(|key| format!("<redacted:{} bytes>", key.bytes.len())),
            )
            .field(
                "receiver_noise_secret_key",
                &format!(
                    "<redacted:{} bytes>",
                    self.receiver_noise_secret_key.bytes.len()
                ),
            )
            .field("live_access", &self.live_access.as_ref().map(|_| "<live>"))
            .finish()
    }
}

#[uniffi::export]
impl FfiPubkySessionAccess {
    /// Create session access material from platform secure storage.
    ///
    /// `client_id` must be the stable app identifier recorded in the exported
    /// grant.
    #[uniffi::constructor]
    pub fn new(
        client_id: String,
        session_secret: String,
        local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
        receiver_noise_secret_key: Arc<FfiReceiverNoiseSecretKey>,
    ) -> Result<Self, PaykitFfiError> {
        let mut session_secret = Zeroizing::new(session_secret);
        ClientId::new(&client_id)
            .map_err(|err| validation_error(format!("invalid Pubky client ID: {err}")))?;
        Ok(Self {
            client_id,
            session_secret: std::mem::take(&mut *session_secret),
            local_secret_key,
            receiver_noise_secret_key,
            live_access: None,
        })
    }

    /// Return the application identifier recorded in the Pubky grant.
    pub fn client_id(&self) -> String {
        self.client_id.clone()
    }

    /// Export the Pubky grant and proof-of-possession secret for secure storage.
    pub fn export_session_secret(&self) -> String {
        self.session_secret.clone()
    }

    /// Export the local Pubky secret key, when available.
    pub fn export_local_secret_key(&self) -> Option<Arc<FfiPubkyLocalSecretKey>> {
        self.local_secret_key.clone()
    }

    /// Export the receiver Noise secret key for platform secure storage.
    pub fn export_receiver_noise_secret_key(&self) -> Arc<FfiReceiverNoiseSecretKey> {
        self.receiver_noise_secret_key.clone()
    }
}

/// Result of creating or importing a Pubky session.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPubkySessionBootstrapResult {
    /// Session access material to persist in platform session storage.
    pub session_access: Arc<FfiPubkySessionAccess>,
    /// Local Pubky public key.
    pub public_key: String,
}

/// Public details parsed from a Pubky auth deep link.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyAuthDetails {
    /// Auth request kind.
    pub kind: FfiPubkyAuthRequestKind,
    /// Requested capabilities as canonical Pubky capability text.
    pub capabilities: String,
    /// Relay URL used by the auth flow.
    pub relay_url: String,
    /// Application identifier that will own the grant.
    pub client_id: String,
    /// Homeserver requested by a signup flow.
    pub homeserver_public_key: Option<String>,
}

/// Sensitive state required to resume a pending Pubky grant auth request.
///
/// Persist this only in secure, temporary platform storage. Delete it after
/// the request completes, expires, or is abandoned.
#[derive(uniffi::Object)]
pub struct FfiPubkyAuthRequestState {
    inner: PubkyAuthRequestState,
}

impl fmt::Debug for FfiPubkyAuthRequestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FfiPubkyAuthRequestState(<redacted>)")
    }
}

#[uniffi::export]
impl FfiPubkyAuthRequestState {
    /// Reconstruct state loaded from secure, temporary platform storage.
    #[uniffi::constructor]
    pub fn new(
        authorization_url: String,
        client_key_secret: Vec<u8>,
    ) -> Result<Self, PaykitFfiError> {
        let mut authorization_url = Zeroizing::new(authorization_url);
        let client_key_secret = Zeroizing::new(client_key_secret);
        let client_key_secret = Zeroizing::new(
            <[u8; 32]>::try_from(client_key_secret.as_slice()).map_err(|_| {
                validation_error(format!(
                    "Pubky auth client key secret must be 32 bytes, got {}",
                    client_key_secret.len()
                ))
            })?,
        );
        Ok(Self {
            inner: PubkyAuthRequestState::new(
                std::mem::take(&mut *authorization_url),
                *client_key_secret,
            )?,
        })
    }

    /// Export the secret-bearing authorization URL for secure persistence.
    pub fn authorization_url(&self) -> String {
        self.inner.authorization_url().to_owned()
    }

    /// Export the proof-of-possession key for secure persistence.
    pub fn export_client_key_secret(&self) -> Vec<u8> {
        self.inner.client_key_secret().to_vec()
    }
}

/// Application-defined input for a Pubky Auth companion claim.
///
/// The application serializes its protocol-specific unsigned payload. Paykit
/// validates the identifiers, creates the request-bound identity signature,
/// encrypts the signed payload, and delivers it before grant approval.
///
/// Generated platform record descriptions may include the raw payload. Apps
/// must not log, interpolate, or otherwise stringify this record.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiPubkyAuthCompanionClaim {
    /// Auth URL query parameter that announces the claim.
    pub query_parameter: String,
    /// Protocol-specific claim type used for URL validation and relay derivation.
    pub claim_type: String,
    /// Protocol-specific unsigned binary payload. Do not log this value.
    pub unsigned_payload: Vec<u8>,
}

impl fmt::Debug for FfiPubkyAuthCompanionClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPubkyAuthCompanionClaim")
            .field("query_parameter", &self.query_parameter)
            .field("claim_type", &self.claim_type)
            .field(
                "unsigned_payload",
                &format_args!("<redacted:{} bytes>", self.unsigned_payload.len()),
            )
            .finish()
    }
}

/// Failure returned while approving Pubky Auth with a companion claim.
#[derive(uniffi::Error, Clone, Debug, thiserror::Error)]
pub enum FfiPubkyAuthCompanionClaimApprovalError {
    /// The URL, claim type, secret, relay, or capability request is invalid.
    #[error("invalid Pubky Auth companion request: {reason}")]
    InvalidAuthUrl { reason: String },
    /// The companion claim description is invalid.
    #[error("invalid Pubky Auth companion claim: {reason}")]
    InvalidClaim { reason: String },
    /// The supplied local Pubky identity key is invalid.
    #[error("invalid local Pubky secret key: {reason}")]
    InvalidLocalSecretKey { reason: String },
    /// XSalsa20-Poly1305 encryption failed before relay delivery.
    #[error("companion claim encryption failed: {reason}")]
    EncryptionFailure { reason: String },
    /// The encrypted companion claim could not be delivered to its relay channel.
    #[error("companion claim relay delivery failed: {reason}")]
    RelayDeliveryFailure { reason: String },
    /// Pubky grant approval failed after companion delivery succeeded.
    #[error("Pubky Auth approval failed after companion delivery: {reason}")]
    AuthorizationFailure { reason: String },
    /// An unknown SDK failure occurred; no claim-delivery state is implied.
    #[error("unexpected Pubky Auth companion claim approval failure: {reason}")]
    Unexpected { reason: String },
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

    /// Clear Pubky session access from local platform storage.
    ///
    /// Normal SDK sign-out revokes the live grant before invoking this callback.
    fn clear_session_access(&self) -> Result<(), PaykitFfiError>;
}

struct CachedPubkySession {
    session_secret_fingerprint: [u8; 32],
    session: pubky::PubkySession,
}

#[derive(Clone)]
pub(crate) struct FfiSdkPubkySessionProviderAdapter {
    provider: Arc<dyn FfiSdkPubkySessionProvider>,
    pubky: Pubky,
    cached_session: Arc<AsyncMutex<Option<CachedPubkySession>>>,
}

impl FfiSdkPubkySessionProviderAdapter {
    pub(crate) fn new(provider: Arc<dyn FfiSdkPubkySessionProvider>, pubky: Pubky) -> Self {
        Self {
            provider,
            pubky,
            cached_session: Arc::new(AsyncMutex::new(None)),
        }
    }
}

#[async_trait]
impl PubkySessionProvider for FfiSdkPubkySessionProviderAdapter {
    async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
        let Some(access) = self
            .provider
            .load_session_access()
            .map_err(|err| ffi_error_to_sdk(err, "load Pubky session access"))?
        else {
            *self.cached_session.lock().await = None;
            return Ok(None);
        };

        let local_secret_key = access
            .local_secret_key
            .clone()
            .map(|key| local_secret_from_bytes(key.export_bytes()))
            .transpose()
            .map_err(|err| ffi_error_to_sdk(err, "load local Pubky secret key"))?;
        let receiver_noise_secret_key = access.receiver_noise_secret_key.export_bytes();
        let receiver_noise_secret_key = receiver_noise_secret_from_bytes(receiver_noise_secret_key)
            .map_err(|err| ffi_error_to_sdk(err, "load receiver Noise secret key"))?;

        let session_secret_fingerprint = Sha256::digest(access.session_secret.as_bytes()).into();
        let mut cached_session = self.cached_session.lock().await;

        if let Some(live_access) = &access.live_access {
            validate_grant_session_client_id(&live_access.session, &access.client_id).await?;
            *cached_session = Some(CachedPubkySession {
                session_secret_fingerprint,
                session: live_access.session.clone(),
            });
            return Ok(Some(PubkySessionAccess {
                session: live_access.session.clone(),
                outbox_client: self.pubky.clone(),
                local_secret_key,
                receiver_noise_secret_key,
            }));
        }

        if let Some(cached) = cached_session
            .as_ref()
            .filter(|cached| cached.session_secret_fingerprint == session_secret_fingerprint)
        {
            validate_grant_session_client_id(&cached.session, &access.client_id).await?;
            return Ok(Some(PubkySessionAccess {
                session: cached.session.clone(),
                outbox_client: self.pubky.clone(),
                local_secret_key,
                receiver_noise_secret_key,
            }));
        }

        let session_secret = Zeroizing::new(access.session_secret.clone());
        let session = self
            .pubky
            .restore_session(&session_secret)
            .await
            .map_err(|err| PaykitSdkError::Identity {
                context: "restore Pubky grant session from platform provider".into(),
                source: Some(err.into()),
            })?;
        validate_grant_session_client_id(&session, &access.client_id).await?;
        *cached_session = Some(CachedPubkySession {
            session_secret_fingerprint,
            session: session.clone(),
        });

        Ok(Some(PubkySessionAccess {
            session,
            outbox_client: self.pubky.clone(),
            local_secret_key,
            receiver_noise_secret_key,
        }))
    }

    async fn revoke_session_access(&self, access: &PubkySessionAccess) -> paykit_sdk::Result<()> {
        let provider_access = self
            .provider
            .load_session_access()
            .map_err(|err| ffi_error_to_sdk(err, "load Pubky session access for revocation"))?
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "cannot revoke Pubky grant without persisted session access".into(),
                source: None,
            })?;
        let bootstrap =
            PubkySessionBootstrap::with_pubky(self.pubky.clone(), &provider_access.client_id)?;

        let mut cached_session = self.cached_session.lock().await;
        bootstrap
            .revoke_grant(&provider_access.session_secret, access)
            .await?;
        *cached_session = None;
        Ok(())
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
            .map_err(|err| ffi_error_to_sdk(err, "clear Pubky session access"))?;
        *self.cached_session.lock().await = None;
        Ok(())
    }
}

async fn validate_grant_session_client_id(
    session: &pubky::PubkySession,
    expected_client_id: &str,
) -> paykit_sdk::Result<()> {
    let grant = session.as_grant().ok_or_else(|| PaykitSdkError::Identity {
        context: "Pubky session must be grant-backed".into(),
        source: None,
    })?;
    let actual_client_id = grant.session_info().await.client_id;
    if actual_client_id.as_str() != expected_client_id {
        return Err(PaykitSdkError::Identity {
            context: format!(
                "Pubky grant client ID `{actual_client_id}` did not match `{expected_client_id}`"
            ),
            source: None,
        });
    }
    Ok(())
}

/// Pubky session bootstrap helper.
#[derive(uniffi::Object)]
pub struct FfiPubkySessionBootstrap {
    inner: PubkySessionBootstrap,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPubkySessionBootstrap {
    /// Create a Pubky session bootstrap helper.
    ///
    /// Reuse `client_id` across auth start, resume, and session import. Grants
    /// issued to another client ID are rejected.
    #[uniffi::constructor]
    pub fn new(client_id: String) -> Result<Self, PaykitFfiError> {
        Self::with_pubky_client_config(client_id, default_pubky_client_config())
    }

    /// Create a Pubky session bootstrap helper with explicit Pubky client configuration.
    #[uniffi::constructor]
    pub fn with_pubky_client_config(
        client_id: String,
        pubky_client: FfiPubkyClientConfig,
    ) -> Result<Self, PaykitFfiError> {
        let mut bootstrap =
            PubkySessionBootstrap::with_pubky(pubky_from_config(&pubky_client)?, &client_id)?;
        if let Some(auth_relay_url) = auth_relay_url(&pubky_client) {
            bootstrap = bootstrap.with_auth_relay(&auth_relay_url)?;
        }
        Ok(Self { inner: bootstrap })
    }

    /// Sign up on a homeserver with the receiver-owned Noise key.
    pub async fn sign_up(
        &self,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
        receiver_noise_secret_key: Arc<FfiReceiverNoiseSecretKey>,
        homeserver_public_key: String,
        signup_code: Option<String>,
        required_capabilities: String,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let secret = local_secret_from_bytes(local_secret_key.export_bytes())?;
        let receiver_noise_secret_key =
            receiver_noise_secret_from_bytes(receiver_noise_secret_key.export_bytes())?;
        let homeserver = parse_public_key(homeserver_public_key)?;
        let result = self
            .inner
            .sign_up(
                &secret,
                receiver_noise_secret_key,
                &homeserver,
                signup_code.as_deref(),
                &required_capabilities,
            )
            .await?;
        bootstrap_result_to_ffi(result, Some(secret)).await
    }

    /// Sign in with the receiver's persisted Noise key.
    pub async fn sign_in(
        &self,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
        receiver_noise_secret_key: Arc<FfiReceiverNoiseSecretKey>,
        required_capabilities: String,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let secret = local_secret_from_bytes(local_secret_key.export_bytes())?;
        let receiver_noise_secret_key =
            receiver_noise_secret_from_bytes(receiver_noise_secret_key.export_bytes())?;
        let result = self
            .inner
            .sign_in(&secret, receiver_noise_secret_key, &required_capabilities)
            .await?;
        bootstrap_result_to_ffi(result, Some(secret)).await
    }

    /// Import an exported Pubky session secret and its persisted receiver Noise key.
    ///
    /// The grant must belong to this bootstrap's client ID and cover every
    /// required capability.
    pub async fn import_session(
        &self,
        session_secret: String,
        local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
        receiver_noise_secret_key: Arc<FfiReceiverNoiseSecretKey>,
        required_capabilities: String,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let session_secret = Zeroizing::new(session_secret);
        let secret = local_secret_key
            .map(|key| local_secret_from_bytes(key.export_bytes()))
            .transpose()?;
        let receiver_noise_secret_key =
            receiver_noise_secret_from_bytes(receiver_noise_secret_key.export_bytes())?;
        let result = self
            .inner
            .import_session(
                &session_secret,
                secret.clone(),
                receiver_noise_secret_key,
                &required_capabilities,
            )
            .await?;
        bootstrap_result_to_ffi(result, secret).await
    }

    /// Start a sign-in auth flow for an external signer.
    pub async fn start_sign_in_auth(
        &self,
        capabilities: String,
    ) -> Result<Arc<FfiPubkyAuthRequest>, PaykitFfiError> {
        Ok(Arc::new(FfiPubkyAuthRequest {
            inner: AsyncMutex::new(Some(self.inner.start_sign_in_auth(&capabilities).await?)),
        }))
    }

    /// Start a signup auth flow for an external signer.
    pub async fn start_sign_up_auth(
        &self,
        capabilities: String,
        homeserver_public_key: String,
        signup_token: Option<String>,
    ) -> Result<Arc<FfiPubkyAuthRequest>, PaykitFfiError> {
        let homeserver = parse_public_key(homeserver_public_key)?;
        Ok(Arc::new(FfiPubkyAuthRequest {
            inner: AsyncMutex::new(Some(
                self.inner
                    .start_sign_up_auth(&capabilities, &homeserver, signup_token)
                    .await?,
            )),
        }))
    }

    /// Resume a short-lived grant auth flow from securely persisted state.
    pub async fn resume_auth(
        &self,
        state: Arc<FfiPubkyAuthRequestState>,
        expected_capabilities: String,
    ) -> Result<Arc<FfiPubkyAuthRequest>, PaykitFfiError> {
        Ok(Arc::new(FfiPubkyAuthRequest {
            inner: AsyncMutex::new(Some(
                self.inner
                    .resume_auth(&state.inner, &expected_capabilities)
                    .await?,
            )),
        }))
    }

    /// Approve a Pubky auth URL with this local secret key.
    ///
    /// The request client ID must match this bootstrap's client ID.
    /// A signup request creates the identity on its requested homeserver before
    /// approving the application grant.
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

    /// Deliver a signed application-defined claim, then approve Pubky Auth.
    ///
    /// This high-level operation owns validation, request-bound signing,
    /// channel derivation, encryption, relay delivery, and approval ordering.
    /// The request client ID must match this bootstrap's client ID.
    pub async fn approve_auth_with_companion_claim(
        &self,
        auth_url: String,
        expected_capabilities: String,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
        claim: FfiPubkyAuthCompanionClaim,
    ) -> Result<(), FfiPubkyAuthCompanionClaimApprovalError> {
        let secret = local_secret_from_bytes(local_secret_key.export_bytes()).map_err(|err| {
            FfiPubkyAuthCompanionClaimApprovalError::InvalidLocalSecretKey {
                reason: err.to_string(),
            }
        })?;
        let claim = PubkyAuthCompanionClaim::new(
            claim.query_parameter,
            claim.claim_type,
            claim.unsigned_payload,
        )
        .map_err(FfiPubkyAuthCompanionClaimApprovalError::from)?;
        self.inner
            .approve_auth_with_companion_claim(&auth_url, &expected_capabilities, &secret, &claim)
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

    /// Export the sensitive state required to resume this pending request.
    pub async fn save_state(&self) -> Result<Arc<FfiPubkyAuthRequestState>, PaykitFfiError> {
        let guard = self.inner.lock().await;
        let state = guard
            .as_ref()
            .ok_or_else(|| validation_error("Pubky auth request already completed"))?
            .save_state()?;
        Ok(Arc::new(FfiPubkyAuthRequestState { inner: state }))
    }

    /// Wait for auth approval using the receiver's persisted Noise key.
    ///
    /// Completion is one-shot, including when the async operation is cancelled
    /// or returns an error. `save_state` can restore an unapproved request
    /// while its relay inbox remains valid. Once completion fetches the
    /// approval, cancellation or a later exchange failure requires a new auth
    /// request.
    pub async fn complete(
        &self,
        local_secret_key: Option<Arc<FfiPubkyLocalSecretKey>>,
        receiver_noise_secret_key: Arc<FfiReceiverNoiseSecretKey>,
        required_capabilities: String,
    ) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
        let request = self
            .inner
            .lock()
            .await
            .take()
            .ok_or_else(|| validation_error("Pubky auth request already completed"))?;
        let secret = local_secret_key
            .map(|key| local_secret_from_bytes(key.export_bytes()))
            .transpose()?;
        let receiver_noise_secret_key =
            receiver_noise_secret_from_bytes(receiver_noise_secret_key.export_bytes())?;
        let result = request
            .complete(
                secret.clone(),
                receiver_noise_secret_key,
                &required_capabilities,
            )
            .await?;
        bootstrap_result_to_ffi(result, secret).await
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
    if let Some(host) = config.local_testnet_host.as_deref() {
        validate_local_testnet_host(host)?;
        builder.testnet_with_host(host);
    }
    builder.request_timeout(Duration::from_secs(config.request_timeout_secs));
    builder
        .build()
        .map(Pubky::with_client)
        .map_err(|err| identity_error("pubky_client", format!("create Pubky client failed: {err}")))
}

fn auth_relay_url(config: &FfiPubkyClientConfig) -> Option<String> {
    config.auth_relay_url.clone().or_else(|| {
        config
            .local_testnet_host
            .as_ref()
            .map(|host| format!("http://{host}:15412/inbox/"))
    })
}

fn validate_local_testnet_host(host: &str) -> Result<(), PaykitFfiError> {
    let is_supported_host = matches!(
        url::Host::parse(host),
        Ok(url::Host::Domain(_) | url::Host::Ipv4(_))
    );
    if host.is_empty() || host.trim() != host || !is_supported_host {
        return Err(validation_error("pubky local testnet host is invalid"));
    }
    Ok(())
}

pub(crate) fn local_secret_from_bytes(
    bytes: Vec<u8>,
) -> Result<PubkyLocalSecretKey, PaykitFfiError> {
    let bytes = Zeroizing::new(bytes);
    let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
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

fn receiver_noise_secret_from_bytes(
    bytes: Vec<u8>,
) -> Result<ReceiverNoiseSecretKey, PaykitFfiError> {
    let bytes = Zeroizing::new(bytes);
    let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        validation_error(format!(
            "receiver Noise secret key must be 32 bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(ReceiverNoiseSecretKey::new(bytes))
}

fn receiver_noise_secret_to_ffi(secret: &ReceiverNoiseSecretKey) -> Arc<FfiReceiverNoiseSecretKey> {
    Arc::new(FfiReceiverNoiseSecretKey::new(secret.as_bytes().to_vec()))
}

async fn bootstrap_result_to_ffi(
    result: PubkySessionBootstrapResult,
    local_secret_key: Option<PubkyLocalSecretKey>,
) -> Result<FfiPubkySessionBootstrapResult, PaykitFfiError> {
    let session_secret = result.export_session_secret().await?.into_inner();
    let receiver_noise_secret_key = &result.access.receiver_noise_secret_key;
    let live_access = result.access.clone();
    Ok(FfiPubkySessionBootstrapResult {
        session_access: Arc::new(FfiPubkySessionAccess {
            client_id: result.client_id,
            session_secret,
            local_secret_key: local_secret_key.as_ref().map(secret_to_ffi),
            receiver_noise_secret_key: receiver_noise_secret_to_ffi(receiver_noise_secret_key),
            live_access: Some(live_access),
        }),
        public_key: app_public_key(&result.public_key),
    })
}

impl From<PubkyAuthRequestKind> for FfiPubkyAuthRequestKind {
    fn from(value: PubkyAuthRequestKind) -> Self {
        match value {
            PubkyAuthRequestKind::SignIn => Self::SignIn,
            PubkyAuthRequestKind::SignUp => Self::SignUp,
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
            client_id: value.client_id,
            homeserver_public_key: value.homeserver_public_key.map(|key| key.to_app_key()),
        }
    }
}

impl From<PubkyAuthCompanionClaimApprovalError> for FfiPubkyAuthCompanionClaimApprovalError {
    fn from(value: PubkyAuthCompanionClaimApprovalError) -> Self {
        match value {
            PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { reason } => {
                Self::InvalidAuthUrl { reason }
            }
            PubkyAuthCompanionClaimApprovalError::InvalidClaim { reason } => {
                Self::InvalidClaim { reason }
            }
            PubkyAuthCompanionClaimApprovalError::EncryptionFailure { reason } => {
                Self::EncryptionFailure { reason }
            }
            PubkyAuthCompanionClaimApprovalError::RelayDeliveryFailure { reason } => {
                Self::RelayDeliveryFailure { reason }
            }
            PubkyAuthCompanionClaimApprovalError::AuthorizationFailure { reason } => {
                Self::AuthorizationFailure { reason }
            }
            _ => Self::Unexpected {
                reason: "unrecognized SDK companion claim approval failure".into(),
            },
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
