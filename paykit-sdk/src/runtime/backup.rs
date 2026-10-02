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
    ///
    /// Restores only into an empty or matching identity-only state backing.
    /// Accounting always requires reconciliation after restore. Retained Allowance
    /// and Payment Request evidence cannot be discarded; rejection changes no state.
    pub async fn restore_backup_state(&self, backup: SdkBackupState) -> Result<RestoreReport> {
        let _identity_guard = self.claim_identity_operation("restore backup")?;
        let _session_guard = Arc::clone(&self.session_operation_gate).write_owned().await;
        let mut trusted_identity = None;
        let mut trusted_noise_public_key = None;
        if backup.local_public_key().is_some() || backup.has_identity_scoped_state() {
            let session_access = self.validate_backup_restore_session(&backup).await?;
            trusted_identity = Some(self.restore_validation_identity(&session_access)?);
            trusted_noise_public_key = session_access
                .paykit_identity_secret_key()
                .as_ref()
                .map(crate::storage::paykit_noise_public_key);
        }
        restore_sdk_backup_state(
            &self.storage,
            backup,
            trusted_identity,
            trusted_noise_public_key,
            self.clock.now(),
        )
        .await
    }

    /// Recover missing or corrupt Pubky shared state from a trusted backup.
    ///
    /// Requires the Pubky identity secret, authorizer capabilities, the protected
    /// current-key authorization, and the current Paykit key and its successor.
    /// Persist the replacement securely first; after success, distribute it to
    /// authorized apps. Healthy state, unreadable generation headers, and unexpected
    /// generations are rejected.
    /// Only corrupt current-generation state can be replaced. All old Noise snapshots and
    /// prepared sends are discarded, and execution requires wallet reconciliation.
    /// Data newer than the backup cannot be recovered by this operation.
    ///
    /// State commits before App Registry and signed key publication. Retry with
    /// the exact same keys and backup: committed replacement-key state is preserved,
    /// even if another app has progressed it. Corrupt replacement-generation state
    /// is rejected, not reset under keys that may already have been used.
    pub async fn recover_shared_state_from_backup(
        &self,
        backup: SdkBackupState,
        replacement_key: crate::PaykitIdentitySecretKey,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        let _identity_guard = self.claim_identity_operation("recover shared state")?;
        let _session_guard = Arc::clone(&self.session_operation_gate).write_owned().await;
        let access = self.validate_backup_restore_session(&backup).await?;
        let current_key =
            access
                .paykit_identity_secret_key()
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "shared-state recovery requires current Paykit key material".into(),
                    source: None,
                })?;
        current_key.validate_successor(&replacement_key)?;
        replacement_key.validate_pubky_derivation(access.local_secret_key.as_ref())?;
        let authorization = noise_key_authorization::validate_rotation_authorization(
            &access,
            &current_key,
            &replacement_key,
        )
        .await?;
        let identity = self.restore_validation_identity(&access)?;
        let owner = access.public_key()?;
        let replacement_noise = crate::storage::paykit_noise_public_key(&replacement_key);
        let (state, _) =
            backup.into_storage_state(Some(&identity), Some(replacement_noise), 0, 0)?;
        let mut state = state.into_storage_state();
        for message in &mut state.outbound_private_messages {
            message.prepared_send = None;
        }
        let now = self.clock.now();
        let (state, _) = crate::storage::run_storage_state_transaction(
            state,
            Box::new(|tx| {
                key_rotation::rotate_private_state(tx, &owner, now)?;
                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
            }),
        )?;
        self.storage
            .recover_shared_state_from_backup(
                current_key.clone(),
                replacement_key.clone(),
                crate::storage::ValidatedStorageState::new(state),
            )
            .await?;

        let current_noise =
            crate::storage::paykit_noise_public_key(&current_key).to_public_key()?;
        let replacement_noise =
            crate::storage::paykit_noise_public_key(&replacement_key).to_public_key()?;
        let registry = self
            .update_paykit_app_registry_for_key_rotation(&access, |registry| {
                key_rotation::apply_registry_key_rotation(
                    registry,
                    &current_key,
                    &replacement_key,
                    &current_noise,
                    &replacement_noise,
                )
            })
            .await?;
        paykit_lib::publish_paykit_noise_key_authorization(&access.session, &authorization).await?;
        Ok(registry)
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
        if backup.has_private_state()
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
