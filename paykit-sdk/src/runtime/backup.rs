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
    pub async fn restore_backup_state(&self, mut backup: SdkBackupState) -> Result<RestoreReport> {
        if backup.identity_public_key().is_some() || backup.has_identity_scoped_state() {
            let session_access = self.pubky.load_session_access().await?.ok_or_else(|| {
                PaykitSdkError::Identity {
                    context:
                        "cannot restore identity-scoped backup without an active Pubky identity"
                            .into(),
                    source: None,
                }
            })?;
            let local_public_key = session_access.public_key()?;
            if backup.identity_public_key() != Some(&local_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "backup identity does not match active Pubky identity".into(),
                    source: None,
                });
            }
            if backup.has_private_identity_scoped_state()
                && session_access.capability() != PubkyIdentityCapability::PrivateLinkCapable
            {
                return Err(PaykitSdkError::Identity {
                    context: "cannot restore private Paykit state without private-link capability"
                        .into(),
                    source: None,
                });
            }
            if let Some(identity_state) = backup.identity_state.as_mut() {
                identity_state.capability = session_access.capability();
                identity_state.local_secret_available = session_access.private_link_capable();
            }
        }
        restore_sdk_backup_state(&self.storage, backup).await
    }
}
