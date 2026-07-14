//! Pubky account, session, and auth-flow helpers.

mod companion_claim;

pub use companion_claim::{
    WatchOnlyAccountAddressType, WatchOnlyAccountClaim, WatchOnlyAccountClaimApprovalError,
    BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY, BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_TYPE,
    BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_VERSION,
};

use std::{fmt, str::FromStr};

use pubky::{
    deep_links::DeepLink, AuthFlowKind, Capabilities, Capability, Pubky, PubkyAuthFlow,
    PubkyResource, PubkySession,
};
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::Zeroize;

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
    /// Export a secret from a signer.
    SecretExport,
}

/// Public details parsed from a Pubky auth deep link.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyAuthDetails {
    /// Auth request kind.
    pub kind: PubkyAuthRequestKind,
    /// Requested capabilities as canonical Pubky capability text.
    pub capabilities: Option<String>,
    /// Relay URL used by the auth flow.
    pub relay_url: Option<String>,
    /// Homeserver requested by a signup flow.
    pub homeserver_public_key: Option<PubkyPublicKey>,
}

impl fmt::Debug for PubkyAuthDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkyAuthDetails")
            .field("kind", &self.kind)
            .field("capabilities", &self.capabilities)
            .field("relay_url", &self.relay_url)
            .field("homeserver_public_key", &self.homeserver_public_key)
            .finish()
    }
}

/// Exported Pubky session bearer secret.
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
    /// Capability implied by the session and optional local secret key.
    pub capability: PubkyIdentityCapability,
}

impl fmt::Debug for PubkySessionBootstrapResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkySessionBootstrapResult")
            .field("access", &"<redacted>")
            .field("public_key", &self.public_key)
            .field("capability", &self.capability)
            .finish()
    }
}

impl PubkySessionBootstrapResult {
    /// Export the bearer secret token used to restore this Pubky session later.
    pub fn export_session_secret(&self) -> PubkySessionSecret {
        PubkySessionSecret::new(self.access.session.export_secret())
    }
}

/// Handle for a pending Pubky auth flow.
pub struct PubkyAuthRequest {
    pubky: Pubky,
    flow: PubkyAuthFlow,
    authorization_url: String,
}

impl PubkyAuthRequest {
    /// Short-lived secret-bearing auth URL to show as a deeplink or QR code.
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Wait for auth approval and validate the resulting session capabilities.
    pub async fn complete(
        self,
        local_secret_key: Option<PubkyLocalSecretKey>,
        required_capabilities: &str,
    ) -> Result<PubkySessionBootstrapResult> {
        validate_auth_url_capabilities(&self.authorization_url, required_capabilities)?;
        let session = self
            .flow
            .await_approval()
            .await
            .map_err(|err| map_pubky_identity_error("complete Pubky auth flow", err))?;
        validate_session_exact_capabilities(&session, required_capabilities)?;
        session_result(session, self.pubky, local_secret_key, required_capabilities)
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
}

impl PubkySessionBootstrap {
    /// Create a bootstrap helper with a default Pubky client.
    pub fn new() -> Result<Self> {
        Ok(Self {
            pubky: Pubky::new()
                .map_err(|err| map_pubky_identity_error("create Pubky client", err))?,
        })
    }

    /// Create a bootstrap helper from an existing Pubky client.
    pub fn with_pubky(pubky: Pubky) -> Self {
        Self { pubky }
    }

    /// Sign up on a homeserver and return validated session access.
    pub async fn sign_up(
        &self,
        secret_key: &PubkyLocalSecretKey,
        homeserver_public_key: &PubkyPublicKey,
        signup_code: Option<&str>,
    ) -> Result<PubkySessionBootstrapResult> {
        let homeserver = homeserver_public_key.to_public_key()?;
        let session = self
            .pubky
            .signer(secret_key.keypair())
            .signup(&homeserver, signup_code)
            .await
            .map_err(|err| map_pubky_identity_error("sign up Pubky session", err))?;
        session_result(
            session,
            self.pubky.clone(),
            Some(secret_key.clone()),
            PAYKIT_SESSION_CAPABILITIES,
        )
    }

    /// Sign in with a local Pubky secret key and return validated session access.
    pub async fn sign_in(
        &self,
        secret_key: &PubkyLocalSecretKey,
    ) -> Result<PubkySessionBootstrapResult> {
        let session = self
            .pubky
            .signer(secret_key.keypair())
            .signin()
            .await
            .map_err(|err| map_pubky_identity_error("sign in Pubky session", err))?;
        session_result(
            session,
            self.pubky.clone(),
            Some(secret_key.clone()),
            PAYKIT_SESSION_CAPABILITIES,
        )
    }

    /// Import an exported Pubky session secret and validate its capabilities.
    pub async fn import_session(
        &self,
        session_secret: &str,
        local_secret_key: Option<PubkyLocalSecretKey>,
        required_capabilities: &str,
    ) -> Result<PubkySessionBootstrapResult> {
        let session =
            PubkySession::import_secret(session_secret, Some(self.pubky.client().clone()))
                .await
                .map_err(|err| map_pubky_identity_error("import Pubky session", err))?;
        session_result(
            session,
            self.pubky.clone(),
            local_secret_key,
            required_capabilities,
        )
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

    /// Resume a short-lived auth flow from its authorization URL.
    ///
    /// The requested capabilities must exactly match `expected_capabilities`.
    pub async fn resume_auth(
        &self,
        authorization_url: &str,
        expected_capabilities: &str,
    ) -> Result<PubkyAuthRequest> {
        validate_sign_in_or_sign_up_auth_url(authorization_url)?;
        validate_auth_url_capabilities(authorization_url, expected_capabilities)?;
        let flow = self
            .pubky
            .resume_auth_flow(authorization_url)
            .map_err(|err| map_pubky_identity_error("resume Pubky auth flow", err))?;
        Ok(PubkyAuthRequest {
            pubky: self.pubky.clone(),
            flow,
            authorization_url: authorization_url.to_string(),
        })
    }

    /// Approve a Pubky auth URL with this local secret key.
    ///
    /// The requested capabilities must exactly match `expected_capabilities`.
    pub async fn approve_auth(
        &self,
        auth_url: &str,
        expected_capabilities: &str,
        secret_key: &PubkyLocalSecretKey,
    ) -> Result<()> {
        validate_sign_in_or_sign_up_auth_url(auth_url)?;
        validate_auth_url_capabilities(auth_url, expected_capabilities)?;
        self.pubky
            .signer(secret_key.keypair())
            .approve_auth(auth_url)
            .await
            .map_err(|err| map_pubky_identity_error("approve Pubky auth flow", err))
    }

    async fn start_auth(&self, capabilities: &str, kind: AuthFlowKind) -> Result<PubkyAuthRequest> {
        let capabilities = parse_capabilities(capabilities)?;
        let flow = self
            .pubky
            .start_auth_flow(&capabilities, kind)
            .map_err(|err| map_pubky_identity_error("start Pubky auth flow", err))?;
        let authorization_url = flow.authorization_url().to_string();
        Ok(PubkyAuthRequest {
            pubky: self.pubky.clone(),
            flow,
            authorization_url,
        })
    }
}

/// Parse an auth deep link into public request details.
pub fn parse_pubky_auth_url(auth_url: &str) -> Result<PubkyAuthDetails> {
    let deep_link = DeepLink::from_str(auth_url)
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky auth URL: {err}")))?;

    match deep_link {
        DeepLink::Signin(link) => Ok(PubkyAuthDetails {
            kind: PubkyAuthRequestKind::SignIn,
            capabilities: Some(parse_auth_url_capabilities(auth_url)?.to_string()),
            relay_url: Some(link.relay().to_string()),
            homeserver_public_key: None,
        }),
        DeepLink::Signup(link) => Ok(PubkyAuthDetails {
            kind: PubkyAuthRequestKind::SignUp,
            capabilities: Some(parse_auth_url_capabilities(auth_url)?.to_string()),
            relay_url: Some(link.relay().to_string()),
            homeserver_public_key: Some(PubkyPublicKey::from_public_key(link.homeserver())),
        }),
        DeepLink::SeedExport(_) => Ok(PubkyAuthDetails {
            kind: PubkyAuthRequestKind::SecretExport,
            capabilities: None,
            relay_url: None,
            homeserver_public_key: None,
        }),
    }
}

/// Resolve a Pubky URI into the transport URL used by Pubky storage.
pub fn resolve_pubky_url(uri: &str) -> Result<String> {
    pubky::resolve_pubky(uri)
        .map(|url| url.to_string())
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky URI: {err}")))
}

/// Parse a `pubky://<public-key>/<path>` resource into stable parts.
pub fn parse_pubky_resource(uri: &str) -> Result<PubkyResourceRef> {
    let resource = PubkyResource::from_str(uri)
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky resource: {err}")))?;
    let public_key = PubkyPublicKey::from_public_key(&resource.owner);
    let path = resource.path.as_str().to_string();
    let normalized = resource.to_pubky_url();
    Ok(PubkyResourceRef {
        public_key,
        path,
        transport_url: resolve_pubky_url(&normalized)?,
    })
}

fn session_result(
    session: PubkySession,
    outbox_client: Pubky,
    local_secret_key: Option<PubkyLocalSecretKey>,
    required_capabilities: &str,
) -> Result<PubkySessionBootstrapResult> {
    let local_secret_key = validate_local_secret_for_session(&session, local_secret_key)?;
    let access = crate::PubkySessionAccess {
        session,
        outbox_client,
        local_secret_key,
    };
    let public_key = access.public_key()?;
    let capability = access.capability_for_capabilities(required_capabilities)?;
    Ok(PubkySessionBootstrapResult {
        access,
        public_key,
        capability,
    })
}

pub(crate) fn parse_capabilities(value: &str) -> Result<Capabilities> {
    let mut capabilities = Vec::new();
    for entry in value.split(',').map(str::trim) {
        if entry.is_empty() {
            return Err(PaykitSdkError::Protocol(
                "Pubky capabilities must not contain empty entries".into(),
            ));
        }
        let capability = Capability::try_from(entry)
            .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky capability: {err}")))?;
        capabilities.push(capability);
    }
    if capabilities.is_empty() {
        return Err(PaykitSdkError::Protocol(
            "Pubky capabilities must contain at least one valid entry".into(),
        ));
    }
    Ok(Capabilities::from(capabilities).normalize())
}

fn validate_sign_in_or_sign_up_auth_url(auth_url: &str) -> Result<()> {
    match parse_pubky_auth_url(auth_url)?.kind {
        PubkyAuthRequestKind::SignIn | PubkyAuthRequestKind::SignUp => Ok(()),
        PubkyAuthRequestKind::SecretExport => Err(PaykitSdkError::Protocol(
            "Pubky secret-export auth URLs cannot be resumed or approved as sessions".into(),
        )),
    }
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
        return Err(PaykitSdkError::Policy(format!(
            "Pubky auth URL requested capabilities `{actual}`, expected `{expected}`"
        )));
    }
    Ok(())
}

fn parse_auth_url_capabilities(auth_url: &str) -> Result<Capabilities> {
    let url = Url::parse(auth_url)
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky auth URL: {err}")))?;
    let mut caps = None;
    for (key, value) in url.query_pairs() {
        if key != "caps" {
            continue;
        }
        if caps.is_some() {
            return Err(PaykitSdkError::Protocol(
                "Pubky auth URL must not contain duplicate caps parameters".into(),
            ));
        }
        caps = Some(value.into_owned());
    }
    let caps =
        caps.ok_or_else(|| PaykitSdkError::Protocol("Pubky auth URL missing caps".into()))?;
    parse_capabilities(&caps)
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

#[cfg(test)]
mod tests;
