//! Pubky account, session, and auth-flow helpers.

mod companion_claim;

pub use companion_claim::{PubkyAuthCompanionClaim, PubkyAuthCompanionClaimApprovalError};

use std::{fmt, str::FromStr};

use pubky::{
    deep_links::DeepLink, AuthFlowKind, Capabilities, Capability, ClientId, GrantAuthFlowState,
    Pubky, PubkyGrantAuthFlow, PubkyResource, PubkySession,
};
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    identity::{PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey},
    PaykitSdkError, Result,
};

/// Default Pubky capabilities needed for Paykit public storage writes.
pub const PAYKIT_SESSION_CAPABILITIES: &str = "/pub/paykit/:rw";

/// Parsed Pubky resource with a normalized owner and path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyResourceRef {
    /// Resource owner.
    pub public_key: PubkyPublicKey,
    /// Absolute resource path.
    pub path: String,
    /// Transport URL resolved by the Pubky client.
    pub transport_url: String,
}

/// Kind of Pubky auth request represented by an auth deep link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PubkyAuthRequestKind {
    /// Sign in to an existing Pubky account.
    SignIn,
    /// Sign up on a Pubky homeserver.
    SignUp,
}

/// Public details parsed from a Pubky auth deep link.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyAuthDetails {
    /// Auth request kind.
    pub kind: PubkyAuthRequestKind,
    /// Requested capabilities as canonical Pubky capability text.
    pub capabilities: String,
    /// Relay URL used by the auth flow.
    pub relay_url: String,
    /// Application identifier that will own the grant.
    pub client_id: String,
    /// Homeserver requested by a signup flow.
    pub homeserver_public_key: Option<PubkyPublicKey>,
}

impl fmt::Debug for PubkyAuthDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkyAuthDetails")
            .field("kind", &self.kind)
            .field("capabilities", &self.capabilities)
            .field("relay_url", &self.relay_url)
            .field("client_id", &self.client_id)
            .field("homeserver_public_key", &self.homeserver_public_key)
            .finish()
    }
}

/// Portable secret material used to restore a Pubky grant session.
///
/// The value contains the signed grant and its proof-of-possession key. Treat
/// it as bearer-equivalent secret material until the grant expires or is
/// revoked.
pub struct PubkySessionSecret(String);

impl PubkySessionSecret {
    fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrow the secret text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the secret text.
    pub fn into_inner(mut self) -> String {
        let value = std::mem::take(&mut self.0);
        self.0.zeroize();
        value
    }
}

impl fmt::Debug for PubkySessionSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PubkySessionSecret(<redacted>)")
    }
}

impl Drop for PubkySessionSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Result of creating or importing a Pubky session for Paykit SDK use.
pub struct PubkySessionBootstrapResult {
    /// Live Pubky session access that can be passed to a `PubkySessionProvider`.
    pub access: crate::PubkySessionAccess,
    /// Local public key for the session.
    pub public_key: PubkyPublicKey,
    /// Application identifier recorded in the grant.
    pub client_id: String,
    /// Capability implied by the session and optional local secret key.
    pub capability: PubkyIdentityCapability,
}

impl fmt::Debug for PubkySessionBootstrapResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkySessionBootstrapResult")
            .field("access", &"<redacted>")
            .field("public_key", &self.public_key)
            .field("client_id", &self.client_id)
            .field("capability", &self.capability)
            .finish()
    }
}

impl PubkySessionBootstrapResult {
    /// Export the secret token used to restore this grant session later.
    pub async fn export_session_secret(&self) -> Result<PubkySessionSecret> {
        let grant = self
            .access
            .session
            .as_grant()
            .ok_or_else(|| unsupported_session_error("Pubky session must be grant-backed"))?;
        let secret = grant.export_local_secret().await.ok_or_else(|| {
            unsupported_session_error("cannot export a delegated Pubky grant session")
        })?;
        Ok(PubkySessionSecret::new(secret))
    }
}

/// Sensitive state required to resume a pending Pubky grant auth request.
///
/// Persist this only in secure, temporary storage and delete it after the
/// request completes, expires, or is abandoned. The authorization URL carries
/// the relay secret and `client_key_secret` is the proof-of-possession key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyAuthRequestState {
    authorization_url: String,
    client_key_secret: [u8; 32],
}

impl PubkyAuthRequestState {
    /// Reconstruct persisted pending-request state.
    pub fn new(authorization_url: String, client_key_secret: [u8; 32]) -> Result<Self> {
        let mut authorization_url = Zeroizing::new(authorization_url);
        let mut client_key_secret = Zeroizing::new(client_key_secret);
        validate_grant_auth_url(&authorization_url)?;
        validate_auth_state_client_key(&authorization_url, &client_key_secret)?;
        Ok(Self {
            authorization_url: std::mem::take(&mut *authorization_url),
            client_key_secret: std::mem::take(&mut *client_key_secret),
        })
    }

    /// Borrow the grant authorization URL.
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Borrow the proof-of-possession secret for secure persistence.
    pub fn client_key_secret(&self) -> &[u8; 32] {
        &self.client_key_secret
    }

    fn from_grant_state(state: GrantAuthFlowState) -> Self {
        Self {
            authorization_url: state.authorization_url,
            client_key_secret: state.client_key_secret,
        }
    }

    fn to_grant_state(&self) -> GrantAuthFlowState {
        GrantAuthFlowState {
            authorization_url: self.authorization_url.clone(),
            client_key_secret: self.client_key_secret,
        }
    }
}

impl fmt::Debug for PubkyAuthRequestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkyAuthRequestState")
            .field("authorization_url", &"<redacted>")
            .field("client_key_secret", &"<redacted>")
            .finish()
    }
}

impl Drop for PubkyAuthRequestState {
    fn drop(&mut self) {
        self.authorization_url.zeroize();
        self.client_key_secret.zeroize();
    }
}

/// Handle for a pending Pubky auth flow.
pub struct PubkyAuthRequest {
    pubky: Pubky,
    flow: PubkyGrantAuthFlow,
    authorization_url: Zeroizing<String>,
}

impl PubkyAuthRequest {
    /// Short-lived secret-bearing auth URL to show as a deeplink or QR code.
    pub fn authorization_url(&self) -> &str {
        self.authorization_url.as_str()
    }

    /// Export the sensitive state required to resume this pending request.
    pub fn save_state(&self) -> Result<PubkyAuthRequestState> {
        self.flow
            .save_local()
            .map(PubkyAuthRequestState::from_grant_state)
            .ok_or_else(|| unsupported_session_error("grant auth flow key is not exportable"))
    }

    /// Wait for auth approval and validate the resulting session capabilities.
    ///
    /// This consumes the request even when approval is cancelled or fails.
    /// [`Self::save_state`] can restore an unapproved request while its relay
    /// inbox remains valid. Once a completion attempt fetches the approval,
    /// cancellation or a later exchange failure requires a new auth request.
    pub async fn complete(
        self,
        local_secret_key: Option<PubkyLocalSecretKey>,
        required_capabilities: &str,
    ) -> Result<PubkySessionBootstrapResult> {
        validate_auth_url_capabilities(&self.authorization_url, required_capabilities)?;
        let client_id = parse_client_id(&parse_pubky_auth_url(&self.authorization_url)?.client_id)?;
        let session = self
            .flow
            .await_approval()
            .await
            .map_err(|err| map_pubky_identity_error("complete Pubky auth flow", err))?;
        validate_session_exact_capabilities(&session, required_capabilities)?;
        session_result(
            session,
            self.pubky,
            local_secret_key,
            required_capabilities,
            &client_id,
        )
        .await
    }
}

impl fmt::Debug for PubkyAuthRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkyAuthRequest")
            .field("authorization_url", &"<redacted>")
            .finish()
    }
}

/// Pubky session bootstrap helper for Paykit integrations.
#[derive(Clone, Debug)]
pub struct PubkySessionBootstrap {
    pubky: Pubky,
    client_id: ClientId,
    auth_relay_url: Option<Url>,
}

impl PubkySessionBootstrap {
    /// Create a bootstrap helper with a default Pubky client.
    ///
    /// `client_id` is the stable application identifier recorded in every
    /// grant, typically a domain name controlled by the integrating app. Reuse
    /// the same value across auth start, resume, and session import; a grant
    /// issued to another client ID is rejected.
    pub fn new(client_id: &str) -> Result<Self> {
        let client_id = parse_client_id(client_id)?;
        let pubky =
            Pubky::new().map_err(|err| map_pubky_identity_error("create Pubky client", err))?;
        Ok(Self {
            pubky,
            client_id,
            auth_relay_url: None,
        })
    }

    /// Create a bootstrap helper from an existing Pubky client.
    pub fn with_pubky(pubky: Pubky, client_id: &str) -> Result<Self> {
        Ok(Self {
            pubky,
            client_id: parse_client_id(client_id)?,
            auth_relay_url: None,
        })
    }

    /// Use an explicit HTTP(S) inbox URL for new Pubky grant auth flows.
    ///
    /// Production integrations normally use Pubky's default relay. This is
    /// useful for local testnets and deployments with a private auth relay.
    pub fn with_auth_relay(mut self, auth_relay_url: &str) -> Result<Self> {
        let auth_relay_url =
            Url::parse(auth_relay_url).map_err(|err| PaykitSdkError::Protocol {
                context: format!("invalid Pubky auth relay URL: {err}"),
                source: None,
            })?;
        if !matches!(auth_relay_url.scheme(), "http" | "https")
            || auth_relay_url.host_str().is_none()
        {
            return Err(PaykitSdkError::Protocol {
                context: "Pubky auth relay must be an absolute HTTP(S) URL".into(),
                source: None,
            });
        }
        self.auth_relay_url = Some(auth_relay_url);
        Ok(self)
    }

    /// Return the stable application identifier used for new grants.
    pub fn client_id(&self) -> &str {
        self.client_id.as_str()
    }

    /// Sign up on a homeserver and return validated session access.
    ///
    /// After creating the account, this uses Pubky grant auth to issue a
    /// session with exactly `required_capabilities`. The auth relay must be
    /// reachable.
    pub async fn sign_up(
        &self,
        secret_key: &PubkyLocalSecretKey,
        homeserver_public_key: &PubkyPublicKey,
        signup_code: Option<&str>,
        required_capabilities: &str,
    ) -> Result<PubkySessionBootstrapResult> {
        let homeserver = homeserver_public_key.to_public_key()?;
        let signer = self.pubky.signer(secret_key.keypair());
        ensure_pubky_account(&signer, &homeserver, signup_code).await?;
        self.sign_in(secret_key, required_capabilities).await
    }

    /// Sign in with a local Pubky secret key and return validated session access.
    ///
    /// This self-approves a standard Pubky grant-auth request so the returned
    /// session has exactly `required_capabilities`. The auth relay must be
    /// reachable.
    pub async fn sign_in(
        &self,
        secret_key: &PubkyLocalSecretKey,
        required_capabilities: &str,
    ) -> Result<PubkySessionBootstrapResult> {
        let request = self.start_sign_in_auth(required_capabilities).await?;
        self.approve_auth(
            request.authorization_url(),
            required_capabilities,
            secret_key,
        )
        .await?;
        request
            .complete(Some(secret_key.clone()), required_capabilities)
            .await
    }

    /// Restore an exported Pubky grant-session secret and validate its access.
    ///
    /// The grant must belong to this bootstrap's client ID and cover every
    /// capability in `required_capabilities`.
    pub async fn import_session(
        &self,
        session_secret: &str,
        local_secret_key: Option<PubkyLocalSecretKey>,
        required_capabilities: &str,
    ) -> Result<PubkySessionBootstrapResult> {
        let session = self
            .pubky
            .restore_session(session_secret)
            .await
            .map_err(|err| map_pubky_identity_error("restore Pubky grant session", err))?;
        session_result(
            session,
            self.pubky.clone(),
            local_secret_key,
            required_capabilities,
            &self.client_id,
        )
        .await
    }

    /// Start a sign-in auth flow for an external signer such as Pubky Ring.
    pub async fn start_sign_in_auth(&self, capabilities: &str) -> Result<PubkyAuthRequest> {
        self.start_auth(capabilities, AuthFlowKind::signin()).await
    }

    /// Start a signup auth flow for an external signer such as Pubky Ring.
    pub async fn start_sign_up_auth(
        &self,
        capabilities: &str,
        homeserver_public_key: &PubkyPublicKey,
        signup_token: Option<String>,
    ) -> Result<PubkyAuthRequest> {
        self.start_auth(
            capabilities,
            AuthFlowKind::signup(homeserver_public_key.to_public_key()?, signup_token),
        )
        .await
    }

    /// Resume a short-lived grant auth flow from securely persisted state.
    ///
    /// The requested capabilities must exactly match `expected_capabilities`.
    pub async fn resume_auth(
        &self,
        state: &PubkyAuthRequestState,
        expected_capabilities: &str,
    ) -> Result<PubkyAuthRequest> {
        validate_auth_url_capabilities(state.authorization_url(), expected_capabilities)?;
        validate_auth_url_client_id(state.authorization_url(), &self.client_id)?;
        let flow = PubkyGrantAuthFlow::restore(state.to_grant_state(), self.pubky.client().clone())
            .map_err(|err| map_pubky_identity_error("resume Pubky auth flow", err))?;
        Ok(PubkyAuthRequest {
            pubky: self.pubky.clone(),
            flow,
            authorization_url: Zeroizing::new(state.authorization_url().to_string()),
        })
    }

    /// Approve a Pubky auth URL with this local secret key.
    ///
    /// The requested capabilities must exactly match `expected_capabilities`.
    /// The request client ID must match this bootstrap's client ID.
    /// A signup request creates the identity on its requested homeserver before
    /// approving the application grant.
    pub async fn approve_auth(
        &self,
        auth_url: &str,
        expected_capabilities: &str,
        secret_key: &PubkyLocalSecretKey,
    ) -> Result<()> {
        validate_grant_auth_url(auth_url)?;
        validate_auth_url_capabilities(auth_url, expected_capabilities)?;
        validate_auth_url_client_id(auth_url, &self.client_id)?;
        let deep_link = DeepLink::from_str(auth_url).map_err(|err| PaykitSdkError::Protocol {
            context: format!("invalid Pubky auth URL: {err}"),
            source: None,
        })?;
        let signer = self.pubky.signer(secret_key.keypair());
        if let DeepLink::SignupGrant(link) = deep_link {
            let params = link.params();
            ensure_pubky_account(&signer, &params.homeserver, params.signup_token.as_deref())
                .await?;
        }
        signer
            .approve_auth(auth_url)
            .await
            .map_err(|err| map_pubky_identity_error("approve Pubky auth flow", err))
    }

    async fn start_auth(&self, capabilities: &str, kind: AuthFlowKind) -> Result<PubkyAuthRequest> {
        let capabilities = parse_capabilities(capabilities)?;
        let mut builder = PubkyGrantAuthFlow::builder(&capabilities, kind, self.client_id.clone())
            .client(self.pubky.client().clone());
        if let Some(auth_relay_url) = &self.auth_relay_url {
            builder = builder.relay(auth_relay_url.clone());
        }
        let flow = builder
            .start()
            .map_err(|err| map_pubky_identity_error("start Pubky auth flow", err))?;
        let authorization_url = Zeroizing::new(flow.authorization_url().to_string());
        Ok(PubkyAuthRequest {
            pubky: self.pubky.clone(),
            flow,
            authorization_url,
        })
    }
}

async fn ensure_pubky_account(
    signer: &pubky::PubkySigner,
    homeserver: &paykit_lib::PublicKey,
    signup_code: Option<&str>,
) -> Result<()> {
    match signer.signup(homeserver, signup_code).await {
        Ok(()) => Ok(()),
        Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
            if status.as_u16() == 409 =>
        {
            signer
                .pkdns()
                .publish_homeserver_force(Some(homeserver))
                .await
                .map_err(|err| map_pubky_identity_error("restore Pubky homeserver record", err))
        }
        Err(err) => Err(map_pubky_identity_error("sign up Pubky identity", err)),
    }
}

/// Parse an auth deep link into public request details.
pub fn parse_pubky_auth_url(auth_url: &str) -> Result<PubkyAuthDetails> {
    let deep_link = DeepLink::from_str(auth_url).map_err(|err| PaykitSdkError::Protocol {
        context: format!("invalid Pubky auth URL: {err}"),
        source: None,
    })?;

    match deep_link {
        DeepLink::SigninGrant(link) => {
            validate_unique_query_parameters(auth_url, &["caps", "relay", "secret", "cid", "cpk"])?;
            let params = link.params();
            Ok(PubkyAuthDetails {
                kind: PubkyAuthRequestKind::SignIn,
                capabilities: params.capabilities.clone().normalize().to_string(),
                relay_url: params.relay.to_string(),
                client_id: params.client_id.to_string(),
                homeserver_public_key: None,
            })
        }
        DeepLink::SignupGrant(link) => {
            validate_unique_query_parameters(
                auth_url,
                &["caps", "relay", "secret", "hs", "st", "cid", "cpk"],
            )?;
            let params = link.params();
            Ok(PubkyAuthDetails {
                kind: PubkyAuthRequestKind::SignUp,
                capabilities: params.capabilities.clone().normalize().to_string(),
                relay_url: params.relay.to_string(),
                client_id: params.client_id.to_string(),
                homeserver_public_key: Some(PubkyPublicKey::from_public_key(&params.homeserver)),
            })
        }
        DeepLink::Signin(_)
        | DeepLink::Signup(_)
        | DeepLink::DirectSignup(_)
        | DeepLink::SeedExport(_) => Err(unsupported_auth_url_error(
            "only Pubky grant auth URLs are supported",
        )),
    }
}

/// Resolve a Pubky URI into the transport URL used by Pubky storage.
pub fn resolve_pubky_url(uri: &str) -> Result<String> {
    pubky::resolve_pubky(uri)
        .map(|url| url.to_string())
        .map_err(|err| PaykitSdkError::Protocol {
            context: format!("invalid Pubky URI: {err}"),
            source: None,
        })
}

/// Parse a `pubky://<public-key>/<path>` resource into stable parts.
pub fn parse_pubky_resource(uri: &str) -> Result<PubkyResourceRef> {
    let resource = PubkyResource::from_str(uri).map_err(|err| PaykitSdkError::Protocol {
        context: format!("invalid Pubky resource: {err}"),
        source: None,
    })?;
    let public_key = PubkyPublicKey::from_public_key(&resource.owner);
    let path = resource.path.as_str().to_string();
    let normalized = resource.to_pubky_url();
    Ok(PubkyResourceRef {
        public_key,
        path,
        transport_url: resolve_pubky_url(&normalized)?,
    })
}

async fn session_result(
    session: PubkySession,
    outbox_client: Pubky,
    local_secret_key: Option<PubkyLocalSecretKey>,
    required_capabilities: &str,
    expected_client_id: &ClientId,
) -> Result<PubkySessionBootstrapResult> {
    validate_grant_session(&session, expected_client_id).await?;
    let local_secret_key = validate_local_secret_for_session(&session, local_secret_key)?;
    let access = crate::PubkySessionAccess {
        session,
        outbox_client,
        local_secret_key,
        paykit_identity_secret_key: None,
    };
    let public_key = access.public_key()?;
    let capability = access.capability_for_capabilities(required_capabilities)?;
    Ok(PubkySessionBootstrapResult {
        access,
        public_key,
        client_id: expected_client_id.to_string(),
        capability,
    })
}

pub(crate) fn parse_capabilities(value: &str) -> Result<Capabilities> {
    let mut capabilities = Vec::new();
    for entry in value.split(',').map(str::trim) {
        if entry.is_empty() {
            return Err(PaykitSdkError::Protocol {
                context: "Pubky capabilities must not contain empty entries".into(),
                source: None,
            });
        }
        let capability = Capability::try_from(entry).map_err(|err| PaykitSdkError::Protocol {
            context: format!("invalid Pubky capability: {err}"),
            source: None,
        })?;
        capabilities.push(capability);
    }
    if capabilities.is_empty() {
        return Err(PaykitSdkError::Protocol {
            context: "Pubky capabilities must contain at least one valid entry".into(),
            source: None,
        });
    }
    Ok(Capabilities::from(capabilities).normalize())
}

fn parse_client_id(value: &str) -> Result<ClientId> {
    ClientId::new(value).map_err(|err| PaykitSdkError::Protocol {
        context: format!("invalid Pubky client ID: {err}"),
        source: None,
    })
}

fn validate_grant_auth_url(auth_url: &str) -> Result<()> {
    parse_pubky_auth_url(auth_url).map(|_| ())
}

fn validate_auth_state_client_key(auth_url: &str, client_key_secret: &[u8; 32]) -> Result<()> {
    let deep_link = DeepLink::from_str(auth_url).map_err(|err| PaykitSdkError::Protocol {
        context: format!("invalid Pubky auth URL: {err}"),
        source: None,
    })?;
    let expected = match deep_link {
        DeepLink::SigninGrant(link) => link.params().client_pk.clone(),
        DeepLink::SignupGrant(link) => link.params().client_pk.clone(),
        _ => {
            return Err(unsupported_auth_url_error(
                "expected a Pubky grant auth URL",
            ))
        }
    };
    let actual = pubky::Keypair::from_secret(client_key_secret).public_key();
    if actual != expected {
        return Err(PaykitSdkError::Protocol {
            context: "Pubky auth request state client key does not match the authorization URL"
                .into(),
            source: None,
        });
    }
    Ok(())
}

fn validate_auth_url_client_id(auth_url: &str, expected_client_id: &ClientId) -> Result<()> {
    let actual = parse_pubky_auth_url(auth_url)?.client_id;
    if actual != expected_client_id.as_str() {
        return Err(PaykitSdkError::Policy {
            context: format!(
                "Pubky auth URL client ID `{actual}` did not match `{expected_client_id}`"
            ),
            source: None,
        });
    }
    Ok(())
}

async fn validate_grant_session(
    session: &PubkySession,
    expected_client_id: &ClientId,
) -> Result<()> {
    let grant = session
        .as_grant()
        .ok_or_else(|| unsupported_session_error("Pubky session must be grant-backed"))?;
    let actual_client_id = grant.session_info().await.client_id;
    if actual_client_id != *expected_client_id {
        return Err(PaykitSdkError::Identity {
            context: format!(
                "Pubky grant client ID `{actual_client_id}` did not match `{expected_client_id}`"
            ),
            source: None,
        });
    }
    Ok(())
}

fn validate_session_exact_capabilities(
    session: &PubkySession,
    expected_capabilities: &str,
) -> Result<()> {
    let actual = Capabilities::from(session.info().capabilities().to_vec()).normalize();
    let expected = parse_capabilities(expected_capabilities)?;
    if actual != expected {
        return Err(PaykitSdkError::Identity {
            context: format!(
                "Pubky session capabilities `{actual}` did not match requested capabilities `{expected}`"
            ),
            source: None,
        });
    }
    Ok(())
}

fn validate_auth_url_capabilities(auth_url: &str, expected_capabilities: &str) -> Result<()> {
    let actual = parse_auth_url_capabilities(auth_url)?;
    let expected = parse_capabilities(expected_capabilities)?;
    if actual != expected {
        return Err(PaykitSdkError::Policy {
            context: format!(
                "Pubky auth URL requested capabilities `{actual}`, expected `{expected}`"
            ),
            source: None,
        });
    }
    Ok(())
}

fn parse_auth_url_capabilities(auth_url: &str) -> Result<Capabilities> {
    parse_capabilities(&parse_pubky_auth_url(auth_url)?.capabilities)
}

fn validate_unique_query_parameters(auth_url: &str, names: &[&str]) -> Result<()> {
    let url = Url::parse(auth_url).map_err(|err| PaykitSdkError::Protocol {
        context: format!("invalid Pubky auth URL: {err}"),
        source: None,
    })?;
    for name in names {
        if url.query_pairs().filter(|(key, _)| key == *name).count() > 1 {
            return Err(PaykitSdkError::Protocol {
                context: format!("Pubky auth URL must not contain duplicate {name} parameters"),
                source: None,
            });
        }
    }
    Ok(())
}

fn validate_local_secret_for_session(
    session: &PubkySession,
    local_secret_key: Option<PubkyLocalSecretKey>,
) -> Result<Option<PubkyLocalSecretKey>> {
    let Some(local_secret_key) = local_secret_key else {
        return Ok(None);
    };
    let session_public_key = PubkyPublicKey::from_public_key(session.info().public_key());
    validate_local_secret_for_public_key(&session_public_key, local_secret_key).map(Some)
}

fn validate_local_secret_for_public_key(
    session_public_key: &PubkyPublicKey,
    local_secret_key: PubkyLocalSecretKey,
) -> Result<PubkyLocalSecretKey> {
    let secret_public_key = local_secret_key.public_key();
    if &secret_public_key != session_public_key {
        return Err(PaykitSdkError::Identity {
            context: "local Pubky secret key does not match session public key".into(),
            source: None,
        });
    }
    Ok(local_secret_key)
}

fn map_pubky_identity_error(context: &'static str, err: pubky::Error) -> PaykitSdkError {
    PaykitSdkError::Identity {
        context: context.into(),
        source: Some(err.into()),
    }
}

fn unsupported_auth_url_error(context: impl Into<String>) -> PaykitSdkError {
    PaykitSdkError::Protocol {
        context: context.into(),
        source: None,
    }
}

fn unsupported_session_error(context: impl Into<String>) -> PaykitSdkError {
    PaykitSdkError::Identity {
        context: context.into(),
        source: None,
    }
}

#[cfg(test)]
mod tests;
