use super::*;
use paykit_lib::PaykitNoiseKeyAuthorization;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Publish this identity's signed current Noise key as its authorizer.
    ///
    /// Requires the local Pubky secret and separate authorizer write capability.
    /// Call before publishing private app capabilities or handing Paykit access
    /// to another app. This initializes or republishes the active generation;
    /// use `rotate_paykit_identity_key` to change it with the shared state.
    pub async fn publish_paykit_noise_key_authorization(
        &self,
    ) -> Result<PaykitNoiseKeyAuthorization> {
        let (access, _) = self.load_session_access_and_refresh_identity().await?;
        let access = access.ok_or_else(|| authorization_error("no active Pubky session"))?;
        let key = access
            .paykit_identity_secret_key()
            .ok_or_else(|| authorization_error("no Paykit identity key"))?;
        let record = sign_authorization(&access, &key)?;
        paykit_lib::publish_paykit_noise_key_authorization(&access.session, &record).await?;
        Ok(record)
    }

    /// Fetch an identity-signed Noise key. Missing records return `NotFound`.
    ///
    /// Returns the homeserver's current record, without modifying local state.
    /// Encrypted Link operations additionally pin peer generations in shared state.
    /// There is no expiry; freshness relies on the homeserver serving current data.
    pub async fn paykit_noise_key_authorization(
        &self,
        owner: PubkyPublicKey,
    ) -> Result<PaykitNoiseKeyAuthorization> {
        let storage = self
            .pubky
            .load_public_storage()
            .await?
            .ok_or_else(|| authorization_error("no Pubky public storage available"))?;
        require_authorization(&storage, &owner.to_public_key()?).await
    }

    pub(super) async fn validate_local_noise_key_authorization(
        &self,
        access: &PubkySessionAccess,
    ) -> Result<()> {
        let key = access
            .paykit_identity_secret_key()
            .ok_or_else(|| authorization_error("no Paykit identity key"))?;
        let record = require_local_authorization(access).await?;
        validate_key(&record, &key)
    }

    pub(super) async fn pin_counterparty_noise_key_authorization(
        &self,
        counterparty: &PubkyPublicKey,
        authorization: &PaykitNoiseKeyAuthorization,
    ) -> Result<()> {
        if authorization.owner() != &counterparty.to_public_key()? {
            return Err(authorization_error(
                "Noise key authorization belongs to another counterparty",
            ));
        }
        self.storage
            .transaction(|tx| {
                let mut peer = tx
                    .linked_peer(counterparty)
                    .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
                if let Some(previous) = &peer.noise_key_authorization {
                    authorization.validate_against(previous)?;
                }
                peer.noise_key_authorization = Some(authorization.clone());
                tx.save_linked_peer(peer);
                Ok(())
            })
            .await
    }
}

pub(super) fn sign_authorization(
    access: &PubkySessionAccess,
    key: &crate::PaykitIdentitySecretKey,
) -> Result<PaykitNoiseKeyAuthorization> {
    access.validate_for_capabilities(crate::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES)?;
    let identity = access.local_secret_key.as_ref().ok_or_else(|| {
        authorization_error("Noise key authorization requires the Pubky identity secret")
    })?;
    key.validate_pubky_derivation(Some(identity))?;
    Ok(PaykitNoiseKeyAuthorization::sign(
        &identity.keypair(),
        crate::storage::paykit_noise_public_key(key).to_public_key()?,
        key.key_generation(),
    )?)
}

pub(super) async fn require_authorization(
    storage: &pubky::PublicStorage,
    owner: &pubky::PublicKey,
) -> Result<PaykitNoiseKeyAuthorization> {
    // Keep the network future out of enclosing handshake and recovery futures.
    Box::pin(paykit_lib::get_paykit_noise_key_authorization(
        storage, owner,
    ))
    .await?
    .ok_or_else(|| PaykitSdkError::NotFound {
        context: format!("no signed Paykit Noise key authorization for {owner}"),
        source: None,
    })
}

async fn require_local_authorization(
    access: &PubkySessionAccess,
) -> Result<PaykitNoiseKeyAuthorization> {
    require_authorization(
        &access.outbox_client.public_storage(),
        access.session.info().public_key(),
    )
    .await
    .map_err(|err| match err {
        PaykitSdkError::NotFound { .. } => {
            authorization_error("missing local signed Paykit Noise key authorization")
        }
        err => err,
    })
}

pub(super) fn validate_key(
    record: &PaykitNoiseKeyAuthorization,
    key: &crate::PaykitIdentitySecretKey,
) -> Result<()> {
    if record.key_generation() != key.key_generation()
        || *record.noise_public_key()
            != crate::storage::paykit_noise_public_key(key).to_public_key()?
    {
        return Err(authorization_error(
            "Paykit key does not match the identity's current signed authorization",
        ));
    }
    Ok(())
}

/// Rotation can be retried after state and authorization have already committed.
pub(super) async fn validate_rotation_authorization(
    access: &PubkySessionAccess,
    current: &crate::PaykitIdentitySecretKey,
    replacement: &crate::PaykitIdentitySecretKey,
) -> Result<PaykitNoiseKeyAuthorization> {
    let replacement_record = sign_authorization(access, replacement)?;
    let current_record = require_local_authorization(access).await?;
    if current_record.key_generation() == replacement.key_generation() {
        validate_key(&current_record, replacement)?;
    } else {
        validate_key(&current_record, current)?;
    }
    Ok(replacement_record)
}

fn authorization_error(context: &str) -> PaykitSdkError {
    PaykitSdkError::Identity {
        context: context.into(),
        source: None,
    }
}
