use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Export SDK-managed backup state.
    pub async fn export_backup_state(&self) -> Result<SdkBackupState> {
        export_sdk_backup_state(&self.storage, self.config.receiver_path.clone()).await
    }

    /// Restore SDK-managed backup state.
    pub async fn restore_backup_state(&self, backup: SdkBackupState) -> Result<RestoreReport> {
        let _identity_guard = self.claim_identity_operation("restore backup")?;
        let mut trusted_identity = None;
        if backup.identity_public_key().is_some() || backup.has_identity_scoped_state() {
            let session_access = self.validate_backup_restore_session(&backup).await?;
            trusted_identity = Some(self.restore_validation_identity(&session_access).await?);
        }
        restore_sdk_backup_state(
            &self.storage,
            backup,
            self.config.receiver_path.clone(),
            trusted_identity,
        )
        .await
    }

    async fn validate_backup_restore_session(
        &self,
        backup: &SdkBackupState,
    ) -> Result<PubkySessionAccess> {
        let session_access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context:
                        "cannot restore identity-scoped backup without an active Pubky identity"
                            .into(),
                    source: None,
                })?;
        let local_public_key = session_access.public_key()?;
        if backup.identity_public_key() != Some(&local_public_key) {
            return Err(PaykitSdkError::Identity {
                context: "backup identity does not match active Pubky identity".into(),
                source: None,
            });
        }
        let receiver_noise_public_key = session_access.receiver_noise_public_key();
        if backup.identity_receiver_noise_public_key() != Some(&receiver_noise_public_key) {
            return Err(PaykitSdkError::Identity {
                context: "backup receiver Noise key does not match active receiver".into(),
                source: None,
            });
        }
        let required_capabilities = self.config.required_session_capabilities();
        session_access.validate_for_capabilities(&required_capabilities)?;
        Ok(session_access)
    }

    async fn restore_validation_identity(
        &self,
        session_access: &PubkySessionAccess,
    ) -> Result<IdentityState> {
        let public_key = session_access.public_key()?;
        let receiver_noise_public_key = session_access.receiver_noise_public_key();
        let required_capabilities = self.config.required_session_capabilities();
        session_access.validate_for_capabilities(&required_capabilities)?;
        let initialized_at = self.clock.now();
        self.storage
            .transaction(move |tx| {
                if let Some(identity) = tx.load_identity_state() {
                    if identity.public_key.as_ref() == Some(&public_key)
                        && identity.receiver_noise_public_key.as_ref()
                            == Some(&receiver_noise_public_key)
                    {
                        return Ok(identity);
                    }
                }
                Ok(IdentityState {
                    public_key: Some(public_key),
                    receiver_noise_public_key: Some(receiver_noise_public_key),
                    initialized_at,
                    sign_out_generation: 0,
                })
            })
            .await
    }
}
