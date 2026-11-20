use crate::{WalletError, WalletStore, WalletStoreSession};
use paykit_sdk::AppScopes;
use pubky_switch_client::app::Protocol;
use pubky_switch_client::store::StoreError;
use pubky_switch_core::{
    enc,
    keys::AppKeys,
    records::{
        app_record_path, check_caps, AppRequest, AppRequestBody, Approval, ApprovalClaim,
        AuthRequest,
    },
    sig::SignedDoc,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Installation information needed to open the user's authentication request.
#[derive(uniffi::Record)]
pub struct EnrollmentInfo {
    /// The 16-byte Switch App ID.
    pub app_id: Vec<u8>,
    /// The public path to include in the requested Homeserver capabilities.
    pub app_record_path: String,
}
/// The original signed app request to deliver with the authentication exchange.
#[derive(uniffi::Record)]
pub struct EnrollmentRequest {
    /// The 16-byte Switch App ID.
    pub app_id: Vec<u8>,
    /// The hash of the original signed request.
    pub request_hash: Vec<u8>,
    /// The signed app request encoded as JSON for the original approval exchange.
    pub signed_request: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
struct PendingRequest {
    signed: SignedDoc,
    secret: [u8; 32],
}
#[derive(Serialize, Deserialize)]
enum EnrollmentState {
    Pending {
        keys: AppKeys,
        request: Option<PendingRequest>,
    },
    Approved {
        keys: AppKeys,
        request: PendingRequest,
        identity: [u8; 32],
        claim: Vec<u8>,
    },
}
impl EnrollmentState {
    fn id(&self) -> [u8; 16] {
        match self {
            Self::Pending { keys, .. } | Self::Approved { keys, .. } => keys.app_id_bytes(),
        }
    }
    async fn read(store: &Arc<WalletStoreSession>) -> Result<Option<Self>, WalletError> {
        store
            .get(b"enrollment")
            .await?
            .map(|v| serde_json::from_slice(&v).map_err(|_| StoreError::Corrupt.into()))
            .transpose()
    }
    async fn save(&self, store: &Arc<WalletStoreSession>) -> Result<(), WalletError> {
        Ok(store
            .put(
                b"enrollment",
                serde_json::to_vec(self).map_err(|_| StoreError::Corrupt)?,
            )
            .await?)
    }
}
pub(crate) fn fixed<const N: usize>(v: Vec<u8>, name: &str) -> Result<[u8; N], WalletError> {
    v.try_into().map_err(|_| WalletError::invalid(name))
}
/// A protected enrollment on this device. The store must exclude other owners across
/// all processes that can reach the same physical namespace.
#[derive(uniffi::Object)]
pub struct WalletEnrollment {
    store: Arc<dyn WalletStore>,
}
#[uniffi::export]
impl WalletEnrollment {
    /// Use the host's durable, fenced device store for enrollment and its original approval.
    #[uniffi::constructor]
    pub fn new(store: Arc<dyn WalletStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }
}
impl WalletEnrollment {
    async fn enrollment_store(
        &self,
        reference: Vec<u8>,
    ) -> Result<Arc<WalletStoreSession>, WalletError> {
        let id = fixed::<16>(reference, "enrollment reference")?;
        Ok(WalletStoreSession::open(
            self.store.clone(),
            format!("paykit-enrollment/{}", enc::b64(&id)),
        )
        .await?)
    }
}
#[uniffi::export(async_runtime = "tokio")]
impl WalletEnrollment {
    /// Store a host-generated random 16-byte reference before starting. Repeating it
    /// returns the same installation and public path after a device or caller restart.
    pub async fn begin_enrollment(
        &self,
        reference: Vec<u8>,
    ) -> Result<EnrollmentInfo, WalletError> {
        let store = self.enrollment_store(reference).await?;
        let state = match EnrollmentState::read(&store).await? {
            Some(state) => state,
            None => {
                let state = EnrollmentState::Pending {
                    keys: AppKeys::generate().map_err(|_| WalletError::RandomUnavailable)?,
                    request: None,
                };
                state.save(&store).await?;
                state
            }
        };
        let id = state.id();
        Ok(EnrollmentInfo {
            app_id: id.to_vec(),
            app_record_path: app_record_path(&id),
        })
    }
    /// Sign the one-time payer request bound to the host's exact Pubky authentication request.
    pub async fn prepare_enrollment(
        &self,
        reference: Vec<u8>,
        client_id: String,
        client_key: Vec<u8>,
        capabilities: String,
        auth_secret: Vec<u8>,
    ) -> Result<EnrollmentRequest, WalletError> {
        let store = self.enrollment_store(reference).await?;
        let (keys, request) = match EnrollmentState::read(&store).await? {
            Some(EnrollmentState::Pending { keys, request }) => (keys, request),
            Some(EnrollmentState::Approved { keys, request, .. }) => (keys, Some(request)),
            None => return Err(WalletError::EnrollmentUnavailable),
        };
        let auth = AuthRequest {
            client_id,
            client_key: fixed(client_key, "auth client key")?,
            caps: capabilities,
            secret: fixed(auth_secret, "auth secret")?,
        };
        let id = keys.app_id_bytes();
        let signed = match request {
            Some(prior) => {
                let req = AppRequest::verify(&prior.signed).map_err(|_| StoreError::Corrupt)?;
                if req.body().app_id != id {
                    return Err(StoreError::Corrupt.into());
                }
                if !req.binds(&auth) || prior.secret != auth.secret {
                    return Err(WalletError::EnrollmentConflict);
                }
                req
            }
            None => {
                // The request is durable once signed. Only the app's own record and
                // optional Switch backup path are requested here (Switch wire 3.9).
                check_caps(&auth.caps, &id, &[])
                    .map_err(|_| WalletError::invalid("capabilities"))?;
                let body = AppRequestBody::new(
                    &keys,
                    AppScopes::new()
                        .pay()
                        .build()
                        .map_err(|error| WalletError::Protocol {
                            reason: error.to_string(),
                        })?
                        .scopes,
                    &auth,
                );
                let req = AppRequest::sign(&keys, body)
                    .map_err(|_| WalletError::invalid("app request"))?;
                EnrollmentState::Pending {
                    keys,
                    request: Some(PendingRequest {
                        signed: req.signed().clone(),
                        secret: auth.secret,
                    }),
                }
                .save(&store)
                .await?;
                req
            }
        };
        Ok(EnrollmentRequest {
            app_id: id.to_vec(),
            request_hash: signed.hash().to_vec(),
            signed_request: serde_json::to_vec(signed.signed()).map_err(|_| StoreError::Corrupt)?,
        })
    }
    /// Keep the selected identity's genuine companion and inner approval in protected
    /// storage before installation. Retrying an already verified approval is idempotent.
    /// A different identity or companion cannot replace the original (Switch wire 3.10).
    pub async fn accept_approval(
        &self,
        reference: Vec<u8>,
        identity: Vec<u8>,
        claim: Vec<u8>,
    ) -> Result<(), WalletError> {
        let identity = fixed(identity, "identity")?;
        let store = self.enrollment_store(reference).await?;
        match EnrollmentState::read(&store).await? {
            Some(EnrollmentState::Approved {
                identity: original,
                claim: previous,
                ..
            }) => {
                if identity != original || claim != previous {
                    return Err(WalletError::EnrollmentConflict);
                }
            }
            Some(EnrollmentState::Pending {
                keys,
                request: Some(request),
            }) => {
                request.verify_approval(&keys, &identity, &claim)?;
                EnrollmentState::Approved {
                    keys,
                    request,
                    identity,
                    claim,
                }
                .save(&store)
                .await?;
            }
            _ => return Err(WalletError::EnrollmentUnavailable),
        }
        Ok(())
    }
}

impl PendingRequest {
    fn verify_approval(
        &self,
        keys: &AppKeys,
        identity: &[u8; 32],
        claim: &[u8],
    ) -> Result<(), WalletError> {
        let request = AppRequest::verify(&self.signed).map_err(|_| StoreError::Corrupt)?;
        if request.body().app_id != keys.app_id_bytes() {
            return Err(StoreError::Corrupt.into());
        }
        let auth = AuthRequest {
            client_id: request.body().client_id.clone(),
            client_key: request.body().client_key,
            caps: request.body().caps.clone(),
            secret: self.secret,
        };
        let companion =
            ApprovalClaim::from_plaintext(claim).map_err(|_| WalletError::ApprovalRefused)?;
        let approval = companion
            .verify(identity, &auth)
            .map_err(|_| WalletError::ApprovalRefused)?;
        let protocol = paykit_sdk::PaykitProtocol::new();
        let approved = Approval::read(
            approval,
            &request,
            identity,
            &keys.bundle_secret(),
            &|scope| protocol.has_keys(scope),
        )
        .map_err(|_| WalletError::ApprovalRefused)?;
        paykit_lib::scopes::check_certificate(&approved.cert)
            .map_err(|_| WalletError::ApprovalRefused)
    }
}
