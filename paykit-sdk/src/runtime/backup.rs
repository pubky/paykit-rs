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
        export_sdk_backup_state(&self.storage).await
    }

    /// Restore SDK-managed backup state.
    pub async fn restore_backup_state(&self, backup: SdkBackupState) -> Result<RestoreReport> {
        let _identity_guard = self.claim_identity_operation("restore backup")?;
        let mut trusted_identity = None;
        if backup.local_public_key().is_some() || backup.has_identity_scoped_state() {
            let session_access = self.validate_backup_restore_session(&backup).await?;
            trusted_identity = Some(self.restore_validation_identity(&session_access)?);
        }
        restore_sdk_backup_state(&self.storage, backup, trusted_identity, self.clock.now()).await
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
        if backup.local_public_key() != Some(&local_public_key) {
            return Err(PaykitSdkError::Identity {
                context: "backup identity does not match active Pubky identity".into(),
                source: None,
            });
        }
        session_access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        if backup.has_identity_scoped_state()
            && session_access.capability_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?
                != PubkyIdentityCapability::PrivateLinkCapable
        {
            return Err(PaykitSdkError::Identity {
                context: "cannot restore private Paykit state without private-link capability"
                    .into(),
                source: None,
            });
        }
        Ok(session_access)
    }

    fn restore_validation_identity(
        &self,
        session_access: &PubkySessionAccess,
    ) -> Result<IdentityState> {
        let public_key = session_access.public_key()?;
        session_access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        Ok(IdentityState {
            public_key: Some(public_key),
            initialized_at: self.clock.now(),
        })
    }
}
