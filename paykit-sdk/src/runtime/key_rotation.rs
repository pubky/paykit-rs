use super::*;

const KEY_ROTATION_RECOVERY_REASON: &str =
    "Paykit identity key rotated; Encrypted Link recovery is required";

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Rotate identity-wide Paykit key material to the next generation.
    ///
    /// The operation preserves contacts, private stream history, Payment
    /// Requests, Receipts, and app-owned records. It removes old Encrypted
    /// Link snapshots and leases, parks unsent private messages for recovery,
    /// and publishes the replacement Noise public key in the App Registry.
    /// Derive `replacement_key` from the Pubky secret for the next generation,
    /// or import that derived key from the identity's authorizer. Persist it
    /// securely before calling this method. Shared
    /// state commits before the App Registry is published, so an error does not
    /// imply rollback. Retry with the exact same current and replacement keys;
    /// do not generate another replacement for the same generation. Distribute
    /// the replacement to remaining authorized applications before they resume.
    /// Rotation rejects live shared peer leases. Expired leases cannot fence
    /// remote requests already dispatched by another application.
    pub async fn rotate_paykit_identity_key(
        &self,
        replacement_key: crate::PaykitIdentitySecretKey,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        let _identity_guard = self.claim_identity_operation("rotate Paykit identity key")?;
        let _session_guard = Arc::clone(&self.session_operation_gate).write_owned().await;
        let access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "cannot rotate Paykit identity key without an active Pubky session"
                        .into(),
                    source: None,
                })?;
        access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        let current_key =
            access
                .paykit_identity_secret_key()
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "cannot rotate Paykit identity key without current key material"
                        .into(),
                    source: None,
                })?;
        current_key.validate_successor(&replacement_key)?;
        replacement_key.validate_pubky_derivation(access.local_secret_key.as_ref())?;

        let owner = access.public_key()?;
        let public_storage = access.outbox_client.public_storage();
        let registry =
            paykit_lib::get_paykit_app_registry(&public_storage, &owner.to_public_key()?)
                .await?
                .ok_or_else(|| PaykitSdkError::NotFound {
                    context: "Paykit app registry".into(),
                    source: None,
                })?;

        let current_noise_public_key = noise_public_key(&current_key);
        let replacement_noise_public_key = noise_public_key(&replacement_key);
        let mut registry_check = registry;
        apply_registry_key_rotation(
            &mut registry_check,
            &current_key,
            &replacement_key,
            &current_noise_public_key,
            &replacement_noise_public_key,
        )?;

        let now = self.clock.now();
        self.storage
            .rotate_paykit_identity_key(
                current_key.clone(),
                replacement_key.clone(),
                move |tx, already_rotated| {
                    if already_rotated {
                        Ok(())
                    } else {
                        rotate_private_state(tx, &owner, now)
                    }
                },
            )
            .await?;

        self.update_paykit_app_registry_for_key_rotation(&access, |registry| {
            apply_registry_key_rotation(
                registry,
                &current_key,
                &replacement_key,
                &current_noise_public_key,
                &replacement_noise_public_key,
            )
        })
        .await
    }
}

pub(super) fn apply_registry_key_rotation(
    registry: &mut paykit_lib::PaykitAppRegistry,
    current_key: &crate::PaykitIdentitySecretKey,
    replacement_key: &crate::PaykitIdentitySecretKey,
    current_noise_public_key: &paykit_lib::PublicKey,
    replacement_noise_public_key: &paykit_lib::PublicKey,
) -> Result<()> {
    match registry.key_generation() {
        generation if generation == current_key.key_generation() => {
            if registry.noise_public_key() != Some(current_noise_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "current Paykit identity key does not match the App Registry".into(),
                    source: None,
                });
            }
            registry.rotate_noise_public_key(
                replacement_noise_public_key.clone(),
                replacement_key.key_generation(),
            )?;
            Ok(())
        }
        generation if generation == replacement_key.key_generation() => {
            if registry.noise_public_key() != Some(replacement_noise_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "replacement Paykit identity key does not match the App Registry"
                        .into(),
                    source: None,
                });
            }
            Ok(())
        }
        generation => Err(PaykitSdkError::Identity {
            context: format!(
                "App Registry key generation {generation} cannot rotate from {} to {}",
                current_key.key_generation(),
                replacement_key.key_generation()
            ),
            source: None,
        }),
    }
}

fn noise_public_key(secret: &crate::PaykitIdentitySecretKey) -> paykit_lib::PublicKey {
    pubky::Keypair::from_secret(&secret.noise_secret_key())
        .public_key()
        .clone()
}

pub(super) fn rotate_private_state(
    tx: &mut dyn StorageTransaction,
    owner: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut state = tx.export_storage_state();
    if state
        .identity_state
        .as_ref()
        .and_then(|identity| identity.public_key.as_ref())
        != Some(owner)
    {
        return Err(PaykitSdkError::Identity {
            context: "shared SDK state does not match the rotating Pubky identity".into(),
            source: None,
        });
    }

    // This check and key replacement share the storage transaction, including
    // across SDK instances. Never revoke a live sender or handshake's lease.
    if state
        .peer_link_operation_leases
        .values()
        .any(|lease| lease.expires_at > now)
    {
        return Err(PaykitSdkError::Policy {
            context: "cannot rotate Paykit identity key while peer link operations are active"
                .into(),
            source: None,
        });
    }

    // Key rotation revokes prepared handoffs, but unresolved usage remains held
    // until the wallet reconciles external execution evidence.
    if let Some(accounting) = state.allowance_accounting.as_mut() {
        crate::domain::allowance_accounting::invalidate_accounting(accounting);
    }
    state.encrypted_link_states.clear();
    state.peer_link_operation_leases.clear();
    for peer in state.linked_peers.values_mut() {
        if !matches!(
            peer.state,
            LinkedPeerState::NotLinked | LinkedPeerState::Blocked
        ) {
            peer.state = LinkedPeerState::RecoveryRequired;
        }
        peer.failure_count = 0;
        peer.local_recovery_attempt_id = None;
        peer.local_recovery_marker_created_at = None;
        peer.local_recovery_marker_last_error = None;
        peer.remote_recovery_attempt_id = None;
        peer.remote_recovery_marker_observed_at = None;
    }
    for message in &mut state.outbound_private_messages {
        if matches!(
            message.status,
            OutboundPrivateMessageStatus::Pending
                | OutboundPrivateMessageStatus::Sending
                | OutboundPrivateMessageStatus::Failed
                | OutboundPrivateMessageStatus::RecoveryRequired
        ) {
            message.status = OutboundPrivateMessageStatus::RecoveryRequired;
            message.updated_at = now;
            message.last_error = Some(KEY_ROTATION_RECOVERY_REASON.into());
            message.prepared_send = None;
        }
    }

    crate::validate_storage_state(&state)?;
    tx.replace_storage_state(crate::storage::ValidatedStorageState::new(state));
    Ok(())
}
