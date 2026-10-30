//! Explicit original-grant logout without refresh or bearer-only fallback.

use std::{fmt, sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use pubky::{GrantClaims, GrantSessionInfo, Keypair, PopNonce, PopProofClaims, PubkySession};
use reqwest::{header::HeaderValue, Method, Request, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard};
use zeroize::Zeroizing;

use super::unsupported_session_error;
use crate::{PubkyPublicKey, Result};

const MAX_INFO_BYTES: usize = 16 * 1024;
const PROOF_LOGOUT_FEATURE: &str = "grant-proof-logout";

/// Acknowledged original-grant logout, not acknowledgement of local erasure.
///
/// This records success from the advertised proof-logout endpoint. It is not a
/// server-signed receipt or evidence that this request was the first revoker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyGrantLogoutReceipt {
    /// Original identity whose grant was revoked.
    pub public_key: PubkyPublicKey,
    /// Original homeserver that acknowledged the proof logout.
    pub homeserver: PubkyPublicKey,
    /// Original grant, including any refreshed bearers belonging to it.
    pub grant_id: String,
    /// Application recorded in that original grant.
    pub client_id: String,
}

struct OriginalGrant {
    client: pubky::PubkyHttpClient,
    http: reqwest::Client,
    icann_http: reqwest::Client,
    session: GrantSessionInfo,
    grant: Zeroizing<String>,
    signer: Keypair,
    execution: Arc<Mutex<Option<PubkyGrantLogoutReceipt>>>,
}

/// Live original-grant authority retained with one removal intent.
///
/// Capture before confirmation while the original owner is admitted. Preparing
/// an attempt only reads support; executing it revokes this original grant even
/// if its bearer refreshed or expired. A replacement grant is never loaded.
/// After an unknown response, only fresh user confirmation permits another
/// attempt with a new nonce. Restart must not reconstruct this authority.
/// Local credentials, cleanup acknowledgements and lifecycle remain the caller's
/// responsibility. This helper never erases local or browser storage.
pub struct PubkyGrantLogout(Arc<OriginalGrant>);

impl fmt::Debug for PubkyGrantLogout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PubkyGrantLogout(<redacted>)")
    }
}

/// One prepared proof logout. Drop to cancel; execution consumes this attempt.
///
/// Retains exclusive admission for this intent until execution or drop, so two
/// confirmations cannot queue duplicate mutations. Cancellation after dispatch
/// may leave an unknown outcome. A new attempt needs fresh user confirmation.
pub struct PubkyGrantLogoutAttempt {
    original: Arc<OriginalGrant>,
    request: Request,
    client: reqwest::Client,
    acknowledgement: OwnedMutexGuard<Option<PubkyGrantLogoutReceipt>>,
}

impl fmt::Debug for PubkyGrantLogoutAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PubkyGrantLogoutAttempt(<redacted>)")
    }
}

impl PubkyGrantLogout {
    /// Capture an existing native grant without importing or exchanging it.
    ///
    /// The dedicated HTTP clients reuse the original PKARR resolver and the
    /// supplied native request timeout, disable redirects and transport retries,
    /// and have no cookie store. ICANN uses the platform TLS verifier; custom
    /// trust anchors from an arbitrary Pubky client are not inherited. Applications
    /// requiring custom ICANN trust must not use this constructor.
    pub async fn capture(session: &PubkySession, request_timeout: Duration) -> Result<Self> {
        let view = session.as_grant().ok_or_else(unavailable)?;
        let info = view.session_info().await;
        let secret = Zeroizing::new(view.export_local_secret().await.ok_or_else(unavailable)?);
        // Validate the existing serialized signer locally; this API makes no
        // authentication request, including when the original grant expired.
        pubky::GrantCredential::from_shared_secret(&secret).map_err(|_| unavailable())?;
        let (grant, signer) = original_signer(&secret, &info)?;
        let current = view.session_info().await;
        if current.grant_id != info.grant_id
            || current.pubky != info.pubky
            || current.client_id != info.client_id
            || current.homeserver != info.homeserver
        {
            return Err(unavailable());
        }
        let http = strict_client(
            reqwest::ClientBuilder::from(session.client().pkarr().clone()),
            request_timeout,
        )?;
        let icann_http = strict_client(reqwest::Client::builder(), request_timeout)?;
        Ok(Self(Arc::new(OriginalGrant {
            client: session.client().clone(),
            http,
            icann_http,
            session: info,
            grant,
            signer,
            execution: Arc::new(Mutex::new(None)),
        })))
    }

    /// Original public binding to retain with the confirmation and local marker.
    pub fn binding(&self) -> PubkyGrantLogoutReceipt {
        self.0.binding()
    }

    /// Prepare one explicitly confirmed attempt without a remote mutation.
    ///
    /// Verifies current support on the captured homeserver. Missing, unavailable
    /// or malformed support fails closed; there is never bearer-only fallback.
    /// Callers must recheck original owner/provider admission after this await
    /// and before executing. Concurrent preparation is rejected, not queued.
    pub async fn prepare_attempt(&self) -> Result<PubkyGrantLogoutAttempt> {
        let acknowledgement =
            self.0.execution.clone().try_lock_owned().map_err(|_| {
                unsupported_session_error("original grant logout is already active")
            })?;
        if acknowledgement.is_some() {
            return Err(unsupported_session_error(
                "original grant logout is already acknowledged",
            ));
        }
        let (client, mut url) = self.0.verify_support().await?;
        url.set_path("/auth/grant/session");
        let request = client
            .request(Method::DELETE, url)
            .header("pubky-host", self.0.session.pubky.z32())
            .build()
            .map_err(|_| unavailable())?;
        Ok(PubkyGrantLogoutAttempt {
            original: Arc::clone(&self.0),
            request,
            client,
            acknowledgement,
        })
    }

    /// Retain a known remote acknowledgement independently of fallible app cleanup.
    /// Returns `None` while execution is active or no response was acknowledged.
    pub fn acknowledged(&self) -> Option<PubkyGrantLogoutReceipt> {
        self.0
            .execution
            .try_lock()
            .ok()
            .and_then(|value| value.clone())
    }
}

impl PubkyGrantLogoutAttempt {
    /// Perform the prepared proof logout once; no retries or local cleanup.
    pub async fn execute(mut self) -> Result<PubkyGrantLogoutReceipt> {
        let now = u64::try_from(chrono::Utc::now().timestamp()).map_err(|_| unavailable())?;
        let claims = PopProofClaims {
            aud: self.original.session.homeserver.clone(),
            gid: self.original.session.grant_id.clone(),
            nonce: PopNonce::generate(),
            iat: now,
        };
        let proof = Zeroizing::new(pubky_common::auth::jws::sign_jws(
            &self.original.signer,
            pubky::POP_JWS_TYP,
            &claims,
        ));
        let body = serde_json::to_vec(&serde_json::json!({
            "grant": self.original.grant.as_str(), "pop": proof.as_str(),
        }))
        .map_err(|_| unavailable())?;
        self.request.headers_mut().insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        *self.request.body_mut() = Some(body.into());
        let response = self
            .client
            .execute(self.request)
            .await
            .map_err(|_| unacknowledged())?;
        if response.status() != StatusCode::OK {
            return Err(unacknowledged());
        }
        // The advertised proof route returns 200 after its transaction commits.
        // Retain that acknowledgement before any further suspension or cleanup.
        let receipt = self.original.binding();
        *self.acknowledgement = Some(receipt.clone());
        Ok(receipt)
    }
}

impl OriginalGrant {
    fn binding(&self) -> PubkyGrantLogoutReceipt {
        PubkyGrantLogoutReceipt {
            public_key: PubkyPublicKey::from_public_key(&self.session.pubky),
            homeserver: PubkyPublicKey::from_public_key(&self.session.homeserver),
            grant_id: self.session.grant_id.to_string(),
            client_id: self.session.client_id.to_string(),
        }
    }

    async fn request(&self, method: Method, path: &str) -> Result<(reqwest::Client, Request)> {
        let url = format!("https://{}{path}", self.session.homeserver.z32())
            .parse()
            .map_err(|_| unavailable())?;
        // Use Pubky's original resolver/transport choice, but never its redirect
        // or retry policy for a proof-bearing request. No session API is called.
        let mut request = self
            .client
            .request_async(method, url)
            .await
            .map_err(|_| unavailable())?
            .build()
            .map_err(|_| unavailable())?;
        if request.url().scheme() != "https" {
            return Err(unavailable());
        }
        request.headers_mut().insert(
            "pubky-host",
            HeaderValue::from_str(&self.session.pubky.z32()).map_err(|_| unavailable())?,
        );
        let transport = if request.url().host_str() == Some(self.session.homeserver.z32().as_str())
        {
            &self.http
        } else {
            &self.icann_http
        };
        Ok((transport.clone(), request))
    }

    async fn verify_support(&self) -> Result<(reqwest::Client, url::Url)> {
        #[derive(Deserialize)]
        struct Info {
            features: Vec<String>,
        }
        let (client, request) = self.request(Method::GET, "/info").await?;
        let url = request.url().clone();
        let mut response = client.execute(request).await.map_err(|_| unavailable())?;
        if response.status() != StatusCode::OK {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_INFO_BYTES - bytes.len() {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let info: Info = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        if !info
            .features
            .iter()
            .any(|feature| feature == PROOF_LOGOUT_FEATURE)
        {
            return Err(unavailable());
        }
        Ok((client, url))
    }
}

fn strict_client(builder: reqwest::ClientBuilder, timeout: Duration) -> Result<reqwest::Client> {
    builder
        .https_only(true)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| unavailable())
}

fn original_signer(secret: &str, info: &GrantSessionInfo) -> Result<(Zeroizing<String>, Keypair)> {
    let parts: Vec<_> = secret.split(':').collect();
    let expected_len = match parts.first().copied() {
        Some("pubky-grant-credential-v1") => 4,
        Some("pubky-grant-credential-v2") => 5,
        _ => return Err(unavailable()),
    };
    if parts.len() != expected_len || parts[1] != info.homeserver.z32() {
        return Err(unavailable());
    }
    let bytes = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| unavailable())?,
    );
    let key = Zeroizing::new(<[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| unavailable())?);
    let signer = Keypair::from_secret(&key);
    let claims = GrantClaims::decode(parts[3]).map_err(|_| unavailable())?;
    if claims.iss != info.pubky
        || claims.jti != info.grant_id
        || claims.client_id != info.client_id
        || claims.cnf != signer.public_key()
    {
        return Err(unavailable());
    }
    Ok((Zeroizing::new(parts[3].to_owned()), signer))
}

fn unavailable() -> crate::PaykitSdkError {
    unsupported_session_error("original grant proof logout is unavailable")
}

fn unacknowledged() -> crate::PaykitSdkError {
    unsupported_session_error("original grant proof logout was not acknowledged")
}

#[cfg(test)]
mod tests;
