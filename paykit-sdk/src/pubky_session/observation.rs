use std::fmt;

use futures_util::StreamExt;
use pubky::{GrantSessionInfo, PubkyHttpClient, PubkySession};
use reqwest::{header::HeaderValue, Method, RequestBuilder, StatusCode};
use zeroize::Zeroizing;

use super::unsupported_session_error;
use crate::{PubkyPublicKey, Result};

const MAX_SESSION_INFO_BYTES: usize = 16 * 1024;

/// Point-in-time evidence about one captured grant, never permission to mutate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PubkyGrantObservation {
    /// The server accepted the captured bearer and returned its exact binding.
    Active,
    /// The original grant's status could not be established.
    ///
    /// This includes expired/rotated bearers, negative HTTP responses, malformed
    /// metadata and transport failures. It does not mean revoked or absent.
    Unknown,
}

/// Read-only observer bound to an already existing live Pubky grant bearer.
///
/// Capture before the original operation and retain the handle with that
/// operation's owner/attempt. Observation never invokes a session provider,
/// credential restore/refresh, shared storage or a mutation endpoint. It cannot
/// reconstruct a lost handle after restart. Its secret bearer is not exportable.
/// Session creation, capabilities and key rotation remain the caller's concern.
pub struct PubkyGrantObserver {
    client: PubkyHttpClient,
    session: GrantSessionInfo,
    bearer: Zeroizing<String>,
}

impl fmt::Debug for PubkyGrantObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PubkyGrantObserver(<redacted>)")
    }
}

impl PubkyGrantObserver {
    /// Capture cached metadata and the current bearer without network access.
    ///
    /// The caller must already hold admission for this exact original session.
    /// A concurrent bearer refresh can make the handle unusable; it must not be
    /// replaced with a newly restored/current session to retry an old operation.
    pub async fn capture(session: &PubkySession) -> Result<Self> {
        let grant = session.as_grant().ok_or_else(|| {
            unsupported_session_error("grant observation requires a live grant session")
        })?;
        let info = grant.session_info().await;
        let bearer = Zeroizing::new(grant.current_bearer().await);
        if bearer.is_empty()
            || info != grant.session_info().await
            || info.grant_id != grant.grant_id().await
        {
            return Err(unsupported_session_error(
                "live grant changed or has no bearer to observe",
            ));
        }
        Ok(Self {
            client: session.client().clone(),
            session: info,
            bearer,
        })
    }

    /// Owner captured from the original live grant.
    pub fn public_key(&self) -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&self.session.pubky)
    }

    /// Client identifier captured from the original live grant.
    pub fn client_id(&self) -> String {
        self.session.client_id.to_string()
    }

    /// Grant identifier captured from the original live grant.
    pub fn grant_id(&self) -> String {
        self.session.grant_id.to_string()
    }

    /// Observe the original grant without authenticating a replacement session.
    ///
    /// Performs one logical GET through the original configured Pubky client;
    /// its transport redirects/retries/timeouts still apply. There is no SDK
    /// retry or credential refresh. A response finishing at another URL is
    /// unknown. Cancellation drops this read without starting cleanup.
    ///
    /// `Active` is not a lease: callers must recheck their original operation's
    /// admission and all other cleanup prerequisites, obtain fresh confirmation,
    /// and preserve uncertainty if subsequent mutations fail.
    pub async fn observe(&self) -> PubkyGrantObservation {
        if !is_unexpired(&self.session) {
            return PubkyGrantObservation::Unknown;
        }
        let url = format!(
            "https://{}/auth/grant/session",
            self.session.homeserver.z32()
        );
        let Ok(url) = url.parse() else {
            return PubkyGrantObservation::Unknown;
        };
        let Ok(request) = self.client.request_async(Method::GET, url).await else {
            return PubkyGrantObservation::Unknown;
        };
        self.observe_request(request).await
    }

    async fn observe_request(&self, builder: RequestBuilder) -> PubkyGrantObservation {
        let (client, request) = builder.build_split();
        let Ok(mut request) = request else {
            return PubkyGrantObservation::Unknown;
        };
        // Replace, rather than append to, the homeserver's inferred routing
        // header. Authentication is addressed to the original owner.
        let Ok(owner) = HeaderValue::from_str(&self.session.pubky.z32()) else {
            return PubkyGrantObservation::Unknown;
        };
        let Ok(mut bearer) = HeaderValue::from_str(&format!("Bearer {}", *self.bearer)) else {
            return PubkyGrantObservation::Unknown;
        };
        bearer.set_sensitive(true);
        request.headers_mut().insert("pubky-host", owner);
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, bearer);
        let expected_url = request.url().clone();
        let Ok(response) = client.execute(request).await else {
            // Do not stringify errors: they can contain private request data.
            return PubkyGrantObservation::Unknown;
        };
        if response.status() != StatusCode::OK
            || response.url() != &expected_url
            || response
                .content_length()
                .is_some_and(|size| size > MAX_SESSION_INFO_BYTES as u64)
        {
            return PubkyGrantObservation::Unknown;
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                return PubkyGrantObservation::Unknown;
            };
            if chunk.len() > MAX_SESSION_INFO_BYTES - body.len() {
                return PubkyGrantObservation::Unknown;
            }
            body.extend_from_slice(&chunk);
        }
        // Mint/restore and GET report different creation timestamps for the
        // same grant. Bind identity and scope; check lifetimes independently.
        match serde_json::from_slice::<GrantSessionInfo>(&body) {
            Ok(info)
                if info.homeserver == self.session.homeserver
                    && info.pubky == self.session.pubky
                    && info.client_id == self.session.client_id
                    && info.grant_id == self.session.grant_id
                    && info.capabilities == self.session.capabilities
                    && is_unexpired(&info) =>
            {
                PubkyGrantObservation::Active
            }
            _ => PubkyGrantObservation::Unknown,
        }
    }
}

fn is_unexpired(info: &GrantSessionInfo) -> bool {
    let Ok(now) = u64::try_from(chrono::Utc::now().timestamp()) else {
        return false;
    };
    now < info.token_expires_at && now < info.grant_expires_at
}

#[cfg(test)]
mod tests;
