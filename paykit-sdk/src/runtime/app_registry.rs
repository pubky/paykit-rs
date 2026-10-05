use super::app_removal::{
    app_removal_blockers, begin_paykit_app_removal, detach_shared_app_reservations,
    retire_app_outbound_private_messages, stage_app_capability_update,
};
use super::*;

const APP_REGISTRY_UPDATE_MAX_ATTEMPTS: usize = 8;

fn app_publication_noop_lease_sequence(
    tx: &dyn StorageTransaction,
    app_id: &paykit_lib::PaykitAppId,
    capabilities: paykit_lib::PaykitAppCapabilities,
) -> Option<u64> {
    if !tx.paykit_app_is_registered(app_id)
        || tx.paykit_app_is_retired(app_id)
        || tx.paykit_app_capabilities(app_id) != Some(capabilities)
        || tx.paykit_app_operation_lease(app_id).is_some()
    {
        return None;
    }
    let state = tx.export_storage_state();
    // Publication reconciles recovery work across apps, including execution claims.
    if !state.payment_request_execution_claims.is_empty()
        || state
            .outbound_private_messages
            .iter()
            .any(|message| message.status == OutboundPrivateMessageStatus::RecoveryRequired)
    {
        return None;
    }
    Some(state.next_paykit_app_operation_lease_id)
}

fn authorized_app_ids(
    apps: &HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>,
    enabled: impl Fn(paykit_lib::PaykitAppCapabilities) -> bool,
) -> Vec<paykit_lib::PaykitAppId> {
    let mut app_ids = apps
        .iter()
        .filter(|(_, capabilities)| enabled(**capabilities))
        .map(|(app_id, _)| app_id.clone())
        .collect::<Vec<_>>();
    app_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    app_ids
}

pub(super) struct CounterpartyAppAuthorizationContext {
    pub(super) registry: Option<paykit_lib::PaykitAppRegistry>,
    pub(super) private_apps: Option<Vec<paykit_lib::PaykitAppId>>,
    pub(super) payment_request_apps: Option<Vec<paykit_lib::PaykitAppId>>,
    pub(super) receipt_apps: Option<Vec<paykit_lib::PaykitAppId>>,
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    pub(super) async fn claim_paykit_app_operation(&self) -> Result<PaykitAppOperationLease> {
        self.claim_paykit_app_operation_inner(false).await
    }

    async fn claim_paykit_app_publication_operation(&self) -> Result<PaykitAppOperationLease> {
        self.claim_paykit_app_operation_inner(true).await
    }

    async fn claim_paykit_app_operation_inner(
        &self,
        allow_unpublished: bool,
    ) -> Result<PaykitAppOperationLease> {
        self.retry_storage_transaction(|| {
            move |tx| self.claim_paykit_app_operation_in_transaction(tx, allow_unpublished)
        })
        .await
    }

    pub(super) fn claim_paykit_app_operation_in_transaction(
        &self,
        tx: &mut dyn StorageTransaction,
        allow_unpublished: bool,
    ) -> Result<PaykitAppOperationLease> {
        let timeout = ChronoDuration::from_std(PAYKIT_APP_OPERATION_LEASE_TIMEOUT)
            .expect("fixed Paykit App operation lease timeout must fit chrono duration");
        let app_id = &self.config.app_id;
        if !allow_unpublished
            && !tx.paykit_app_is_registered(app_id)
            && !tx.paykit_app_is_retired(app_id)
        {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "Paykit app '{app_id}' must be published before claiming shared work"
                ),
                source: None,
            });
        }
        let now = self.clock.now();
        tx.claim_paykit_app_operation(app_id, now, now + timeout)?
            .ok_or_else(|| PaykitSdkError::Policy {
                context: format!("Paykit App operation already in progress for '{app_id}'"),
                source: None,
            })
    }

    pub(super) async fn release_paykit_app_operation(
        &self,
        lease: &PaykitAppOperationLease,
    ) -> Result<()> {
        self.retry_storage_transaction(|| {
            let lease = lease.clone();
            move |tx| {
                tx.release_paykit_app_operation(&lease.app_id, lease.lease_id);
                Ok(())
            }
        })
        .await
    }

    pub(super) async fn require_paykit_app_operation_lease(
        &self,
        lease: &PaykitAppOperationLease,
    ) -> Result<()> {
        self.retry_storage_transaction(|| {
            let lease = lease.clone();
            move |tx| {
                crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
                let timeout = ChronoDuration::from_std(PAYKIT_APP_OPERATION_LEASE_TIMEOUT)
                    .expect("fixed Paykit App lease timeout must fit chrono duration");
                let now = self.clock.now();
                if tx
                    .paykit_app_operation_lease(&lease.app_id)
                    .is_some_and(|active| active.expires_at <= now + timeout / 2)
                {
                    tx.renew_paykit_app_operation(&lease.app_id, lease.lease_id, now + timeout);
                }
                Ok(())
            }
        })
        .await
    }

    pub(super) async fn finish_paykit_app_operation<T>(
        &self,
        lease: PaykitAppOperationLease,
        result: Result<T>,
    ) -> Result<T> {
        // A failed release leaves an expiring lease, not an uncommitted operation.
        let _ = self.release_paykit_app_operation(&lease).await;
        result
    }

    pub(super) async fn counterparty_app_authorization_context(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<CounterpartyAppAuthorizationContext> {
        if let Some(public_storage) = self.pubky.load_public_storage().await? {
            let registry = paykit_lib::get_paykit_app_registry(
                &public_storage,
                &counterparty.to_public_key()?,
            )
            .await?;
            let apps = registry
                .as_ref()
                .map(|registry| {
                    registry
                        .apps()
                        .iter()
                        .map(|(app_id, app)| (app_id.clone(), app.capabilities()))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            let private_apps =
                authorized_app_ids(&apps, |capabilities| capabilities.private_payments);
            let payment_request_apps =
                authorized_app_ids(&apps, |capabilities| capabilities.payment_requests);
            let receipt_apps = authorized_app_ids(&apps, |capabilities| capabilities.receipts);
            self.storage
                .transaction({
                    let counterparty = counterparty.clone();
                    move |tx| {
                        if tx
                            .load_identity_state()
                            .is_some_and(|state| state.public_key.is_some())
                        {
                            tx.save_authorized_paykit_apps(counterparty, apps);
                        }
                        Ok(())
                    }
                })
                .await?;
            Ok(CounterpartyAppAuthorizationContext {
                registry,
                private_apps: Some(private_apps),
                payment_request_apps: Some(payment_request_apps),
                receipt_apps: Some(receipt_apps),
            })
        } else {
            self.cached_counterparty_app_authorization_context(counterparty)
                .await
        }
    }

    pub(super) async fn cached_counterparty_app_authorization_context(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<CounterpartyAppAuthorizationContext> {
        let apps = self
            .storage
            .transaction(|tx| Ok(tx.authorized_paykit_apps(counterparty)))
            .await?;
        Ok(CounterpartyAppAuthorizationContext {
            registry: None,
            private_apps: apps
                .as_ref()
                .map(|apps| authorized_app_ids(apps, |capabilities| capabilities.private_payments)),
            payment_request_apps: apps
                .as_ref()
                .map(|apps| authorized_app_ids(apps, |capabilities| capabilities.payment_requests)),
            receipt_apps: apps
                .as_ref()
                .map(|apps| authorized_app_ids(apps, |capabilities| capabilities.receipts)),
        })
    }

    pub(super) async fn authorized_receipt_apps_for_peer(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<Vec<paykit_lib::PaykitAppId>>> {
        Ok(self
            .counterparty_app_authorization_context(counterparty)
            .await?
            .receipt_apps)
    }

    /// Fetch the public Paykit application registry for an identity.
    pub async fn paykit_app_registry(
        &self,
        owner: PubkyPublicKey,
    ) -> Result<Option<paykit_lib::PaykitAppRegistry>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available".into(),
                    source: None,
                })?;
        Ok(paykit_lib::get_paykit_app_registry(&public_storage, &owner.to_public_key()?).await?)
    }

    /// Add or replace this application in the identity-wide registry.
    ///
    /// Publishing also reactivates an application whose earlier removal did
    /// not complete. After a failed publication, staged capability restrictions
    /// remain in effect until a successful publication reconciles them.
    pub async fn publish_paykit_app(
        &self,
        app: paykit_lib::PaykitApp,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        let _identity_guard = self.claim_identity_operation("publish Paykit app")?;
        let (session, _, noop_sequence) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                Ok(app_publication_noop_lease_sequence(
                    tx,
                    &self.config.app_id,
                    app.capabilities(),
                ))
            })
            .await?;
        let session = session.ok_or_else(|| PaykitSdkError::Identity {
            context: "publishing a Paykit app requires an active Pubky session".into(),
            source: None,
        })?;
        // Publication has no wallet callbacks. Keep its durable transactions
        // under one storage lock; registry writes never acquire shared storage.
        self.with_guarded_storage_operation(
            Arc::clone(&session._guard),
            Box::pin(async {
                if let Some(sequence) = noop_sequence.flatten() {
                    if let Some(registry) =
                        self.unchanged_paykit_app(&app, &session, sequence).await?
                    {
                        return Ok(registry);
                    }
                }
                let app_lease = self.claim_paykit_app_publication_operation().await?;
                let result = self
                    .publish_paykit_app_inner(app, &app_lease, &session)
                    .await;
                self.finish_paykit_app_operation(app_lease, result).await
            }),
        )
        .await
    }

    async fn unchanged_paykit_app(
        &self,
        app: &paykit_lib::PaykitApp,
        session_access: &PubkySessionAccess,
        lease_sequence: u64,
    ) -> Result<Option<paykit_lib::PaykitAppRegistry>> {
        let (registry, _) = self
            .load_paykit_app_registry_for_update(session_access, true)
            .await?;
        let expected_noise_key = session_access.paykit_identity_secret_key().map(|key| {
            (
                pubky::Keypair::from_secret(&key.noise_secret_key()).public_key(),
                key.key_generation(),
            )
        });
        // Loading validates authorization but may initialize a key only on its clone.
        if registry.apps().get(&self.config.app_id) != Some(app)
            || expected_noise_key
                .as_ref()
                .is_some_and(|(key, generation)| {
                    registry.noise_public_key() != Some(key)
                        || registry.key_generation() != *generation
                })
        {
            return Ok(None);
        }

        let owner = session_access.public_key()?;
        let noise_key = expected_noise_key
            .as_ref()
            .map(|(key, _)| PubkyPublicKey::from_public_key(key));
        // Generic adapters need not lock the whole operation. Recheck atomically,
        // including a lease claimed and released while the registry was read.
        let unchanged = self
            .storage
            .transaction(|tx| {
                Ok(
                    tx.load_identity_state().and_then(|state| state.public_key) == Some(owner)
                        && noise_key
                            .as_ref()
                            .is_none_or(|key| tx.paykit_noise_public_key().as_ref() == Some(key))
                        && app_publication_noop_lease_sequence(
                            tx,
                            &self.config.app_id,
                            app.capabilities(),
                        ) == Some(lease_sequence),
                )
            })
            .await?;
        Ok(unchanged.then_some(registry))
    }

    async fn publish_paykit_app_inner(
        &self,
        app: paykit_lib::PaykitApp,
        app_lease: &PaykitAppOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        let app_id = self.config.app_id.clone();
        let capabilities = app.capabilities();
        let (registry, revision) = self
            .load_paykit_app_registry_for_update(session_access, true)
            .await?;
        self.require_paykit_app_operation_lease(app_lease).await?;
        let remote_capabilities = registry
            .apps()
            .get(&app_id)
            .map(|previous| previous.capabilities());
        stage_app_capability_update(
            &self.storage,
            app_lease,
            remote_capabilities,
            capabilities,
            self.clock.now(),
        )
        .await?;
        // Keep the staged restrictions on failure: publication may have committed.
        let registry = self
            .update_paykit_app_registry_with_access_inner(
                session_access,
                true,
                true,
                Some(app_lease),
                Some((registry, revision)),
                |registry| {
                    registry.register_app(app_id.clone(), app.clone())?;
                    Ok(())
                },
            )
            .await?;
        let now = self.clock.now();
        self.storage
            .transaction({
                let app_lease = app_lease.clone();
                move |tx| {
                    crate::storage::require_paykit_app_operation_lease(tx, &app_lease)?;
                    tx.save_paykit_app_capabilities(&app_id, capabilities);
                    tx.activate_paykit_app(&app_id);
                    let linked_counterparties = tx
                        .export_storage_state()
                        .linked_peers
                        .into_values()
                        .filter(|peer| peer.state == LinkedPeerState::Linked)
                        .map(|peer| peer.counterparty)
                        .collect::<Vec<_>>();
                    for counterparty in linked_counterparties {
                        requeue_recovery_required_outbound_messages(tx, &counterparty, now)?;
                    }
                    Ok(())
                }
            })
            .await?;
        Ok(registry)
    }

    /// Remove this application's public Payment Endpoints and registry entry.
    ///
    /// Removal requires app-owned Payment Requests and private financial events
    /// to be complete. It then blocks new app-owned private work before cleanup
    /// begins. If cleanup fails, call this method again or publish the app to
    /// reactivate it.
    pub async fn remove_paykit_app(&self) -> Result<paykit_lib::PaykitAppRegistry> {
        let _identity_guard = self.claim_identity_operation("remove Paykit app")?;
        self.load_session_access_and_refresh_identity().await?;
        let app_lease = self.claim_paykit_app_operation().await?;
        let result = self.remove_paykit_app_inner(&app_lease).await;
        self.finish_paykit_app_operation(app_lease, result).await
    }

    async fn remove_paykit_app_inner(
        &self,
        app_lease: &PaykitAppOperationLease,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        let session_access = self.paykit_app_registry_session_access().await?;
        self.load_paykit_app_registry_for_update(&session_access, true)
            .await?;
        let app_id = self.config.app_id.clone();
        self.require_paykit_app_operation_lease(app_lease).await?;
        let blockers = begin_paykit_app_removal(&self.storage, app_lease, self.clock.now()).await?;
        if !blockers.is_empty() {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "cannot remove Paykit app while it owns {} active Payment Request(s), {} undelivered private event(s), {} incomplete Receipt issuance(s), and {} shared Private Payment List(s); cancel, finish, or clear them before retrying",
                    blockers.active_payment_requests,
                    blockers.undelivered_private_events,
                    blockers.incomplete_receipt_issuances,
                    blockers.shared_private_payment_lists,
                ),
                source: None,
            });
        }
        let lease_timeout = ChronoDuration::from_std(PEER_LINK_OPERATION_LEASE_TIMEOUT)
            .expect("fixed peer link lease timeout must fit chrono duration");
        let leases = self
            .storage
            .transaction({
                let app_lease = app_lease.clone();
                move |tx| {
                    let now = self.clock.now();
                    retire_app_outbound_private_messages(tx, &app_lease, now, now + lease_timeout)
                }
            })
            .await?;
        let cleanup_result = async {
            let mut cleanup_failures = Vec::new();
            for lease in &leases {
                self.require_paykit_app_operation_lease(app_lease).await?;
                cleanup_failures.extend(
                    self.cancel_terminal_private_list_reservations(
                        &lease.counterparty,
                        Some(lease),
                        Some(app_lease),
                    )
                    .await,
                );
            }
            if !cleanup_failures.is_empty() {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "cannot remove Paykit app because {} private reservation cleanup operation(s) failed",
                        cleanup_failures.len()
                    ),
                    source: None,
                });
            }

            let remaining_reservations = self
                .storage
                .transaction({
                    let app_lease = app_lease.clone();
                    move |tx| detach_shared_app_reservations(tx, &app_lease)
                })
                .await?;
            if remaining_reservations != 0 {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "cannot remove Paykit app because {remaining_reservations} private reservation cleanup operation(s) remain"
                    ),
                    source: None,
                });
            }

            let mut identifiers = paykit_lib::list_payment_endpoint_identifiers(
                &session_access.outbox_client.public_storage(),
                session_access.session.info().public_key(),
                &app_id,
            )
            .await?
                .into_iter()
                .collect::<HashSet<_>>();
            for record in self
                .storage
                .transaction(|tx| Ok(tx.public_endpoint_records()))
                .await?
                .into_iter()
                .filter(|record| {
                    record.app_id == app_id && record.status != PublicationStatus::Removed
                })
            {
                identifiers.insert(paykit_lib::PaymentEndpointIdentifier::new(
                    record.identifier,
                )?);
            }
            let mut identifiers = identifiers.into_iter().collect::<Vec<_>>();
            identifiers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            for identifier in identifiers {
                self.require_paykit_app_operation_lease(app_lease).await?;
                self.remove_public_endpoint_if_current(&session_access, &identifier, None, app_lease)
                    .await?;
            }

            self.storage
                .transaction({
                    let app_id = app_id.clone();
                    let app_lease = app_lease.clone();
                    move |tx| {
                        crate::storage::require_paykit_app_operation_lease(tx, &app_lease)?;
                        let now = self.clock.now();
                        for record in tx
                            .public_endpoint_records()
                            .into_iter()
                            .filter(|record| record.app_id == app_id)
                        {
                            tx.save_public_endpoint_record(removed_record(
                                &app_id,
                                record.identifier,
                                now,
                            ));
                        }
                        Ok(())
                    }
                })
                .await?;

            self.require_paykit_app_operation_lease(app_lease).await?;
            let registry = self
                .update_paykit_app_registry_with_access_inner(&session_access, true, true, Some(app_lease), None, |registry| {
                    registry.remove_app(&app_id);
                    Ok(())
                })
                .await?;
            Ok(registry)
        }
        .await;

        for lease in &leases {
            let _ = self.release_peer_link_operation(lease).await;
        }
        cleanup_result
    }

    /// Report work that must finish before this application can be removed.
    pub async fn paykit_app_removal_blockers(&self) -> Result<PaykitAppRemovalBlockers> {
        app_removal_blockers(&self.storage, &self.config.app_id, self.clock.now()).await
    }

    /// Set or clear the identity-wide default Paykit application.
    pub async fn set_default_paykit_app(
        &self,
        app_id: Option<paykit_lib::PaykitAppId>,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        self.update_paykit_app_registry("set default Paykit app", false, move |registry| {
            registry.set_default_app(app_id.clone())?;
            Ok(())
        })
        .await
    }

    /// Set or clear the default Paykit application for one endpoint identifier.
    pub async fn set_default_paykit_app_for_endpoint(
        &self,
        identifier: paykit_lib::PaymentEndpointIdentifier,
        app_id: Option<paykit_lib::PaykitAppId>,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        self.update_paykit_app_registry(
            "set default Paykit app for endpoint",
            false,
            move |registry| {
                if let Some(app_id) = app_id.as_ref() {
                    registry.set_default_app_for_endpoint(identifier.clone(), app_id.clone())?;
                } else {
                    registry.clear_default_app_for_endpoint(&identifier);
                }
                Ok(())
            },
        )
        .await
    }

    async fn update_paykit_app_registry<F>(
        &self,
        operation: &'static str,
        create_if_missing: bool,
        update: F,
    ) -> Result<paykit_lib::PaykitAppRegistry>
    where
        F: Fn(&mut paykit_lib::PaykitAppRegistry) -> Result<()>,
    {
        let _identity_guard = self.claim_identity_operation(operation)?;
        let session_access = self.paykit_app_registry_session_access().await?;
        self.update_paykit_app_registry_with_access(&session_access, create_if_missing, update)
            .await
    }

    async fn paykit_app_registry_session_access(&self) -> Result<GuardedSessionAccess> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })
    }

    async fn load_paykit_app_registry_for_update(
        &self,
        session_access: &PubkySessionAccess,
        create_if_missing: bool,
    ) -> Result<(paykit_lib::PaykitAppRegistry, Option<String>)> {
        let public_storage = session_access.outbox_client.public_storage();
        let session_info = session_access.session.info();
        let owner = session_info.public_key();
        let existing =
            paykit_lib::get_paykit_app_registry_with_revision(&public_storage, owner).await?;
        let (registry, revision) = match existing {
            Some((registry, revision)) => (registry, Some(revision)),
            None if create_if_missing => (paykit_lib::PaykitAppRegistry::new(None), None),
            None => {
                return Err(PaykitSdkError::NotFound {
                    context: "Paykit app registry".into(),
                    source: None,
                });
            }
        };
        // Validation may add the local Noise key. Keep the original for no-op detection.
        self.validate_local_registry_noise_key(session_access, &mut registry.clone())
            .await?;
        Ok((registry, revision))
    }

    pub(super) async fn update_paykit_app_registry_with_access<F>(
        &self,
        session_access: &PubkySessionAccess,
        create_if_missing: bool,
        update: F,
    ) -> Result<paykit_lib::PaykitAppRegistry>
    where
        F: Fn(&mut paykit_lib::PaykitAppRegistry) -> Result<()>,
    {
        self.update_paykit_app_registry_with_access_inner(
            session_access,
            create_if_missing,
            true,
            None,
            None,
            update,
        )
        .await
    }

    pub(super) async fn update_paykit_app_registry_for_key_rotation<F>(
        &self,
        session_access: &PubkySessionAccess,
        update: F,
    ) -> Result<paykit_lib::PaykitAppRegistry>
    where
        F: Fn(&mut paykit_lib::PaykitAppRegistry) -> Result<()>,
    {
        self.update_paykit_app_registry_with_access_inner(
            session_access,
            false,
            false,
            None,
            None,
            update,
        )
        .await
    }

    async fn update_paykit_app_registry_with_access_inner<F>(
        &self,
        session_access: &PubkySessionAccess,
        create_if_missing: bool,
        validate_local_noise_key: bool,
        app_lease: Option<&PaykitAppOperationLease>,
        mut initial_registry: Option<(paykit_lib::PaykitAppRegistry, Option<String>)>,
        update: F,
    ) -> Result<paykit_lib::PaykitAppRegistry>
    where
        F: Fn(&mut paykit_lib::PaykitAppRegistry) -> Result<()>,
    {
        let public_storage = session_access.outbox_client.public_storage();
        let session_info = session_access.session.info();
        let owner = session_info.public_key();
        for attempt in 0..APP_REGISTRY_UPDATE_MAX_ATTEMPTS {
            let (mut registry, revision) = if let Some(initial) = initial_registry.take() {
                initial
            } else {
                let snapshot =
                    paykit_lib::get_paykit_app_registry_with_revision(&public_storage, owner)
                        .await?;
                match snapshot {
                    Some((registry, revision)) => (registry, Some(revision)),
                    None if create_if_missing => (paykit_lib::PaykitAppRegistry::new(None), None),
                    None => {
                        return Err(PaykitSdkError::NotFound {
                            context: "Paykit app registry".into(),
                            source: None,
                        });
                    }
                }
            };
            let unchanged = registry.clone();
            if validate_local_noise_key {
                self.validate_local_registry_noise_key(session_access, &mut registry)
                    .await?;
            }
            update(&mut registry)?;
            if let Some(lease) = app_lease {
                self.require_paykit_app_operation_lease(lease).await?;
            }
            if registry == unchanged {
                return Ok(registry);
            }
            let write = match revision {
                Some(revision) => {
                    paykit_lib::update_paykit_app_registry(
                        &session_access.session,
                        &registry,
                        &revision,
                    )
                    .await
                }
                None => {
                    paykit_lib::create_paykit_app_registry(&session_access.session, &registry).await
                }
            };
            match write {
                Ok(()) => return Ok(registry),
                Err(err)
                    if paykit_lib::is_write_conflict(&err)
                        && attempt + 1 < APP_REGISTRY_UPDATE_MAX_ATTEMPTS =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(25 * (attempt as u64 + 1)))
                        .await;
                    continue;
                }
                Err(err) if paykit_lib::is_write_conflict(&err) => {
                    return Err(PaykitSdkError::ConcurrentUpdate {
                        context: format!(
                            "Paykit App Registry remained busy after {APP_REGISTRY_UPDATE_MAX_ATTEMPTS} update attempts"
                        ),
                        source: Some(err.into()),
                    });
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("bounded App Registry update loop always returns")
    }

    async fn validate_local_registry_noise_key(
        &self,
        session_access: &PubkySessionAccess,
        registry: &mut paykit_lib::PaykitAppRegistry,
    ) -> Result<()> {
        if let Some(secret) = session_access.paykit_identity_secret_key() {
            self.validate_local_noise_key_authorization(session_access)
                .await?;
            let local_noise_public_key =
                pubky::Keypair::from_secret(&secret.noise_secret_key()).public_key();
            registry
                .set_noise_public_key(local_noise_public_key, secret.key_generation())
                .map_err(|_| PaykitSdkError::Identity {
                    context: "Paykit App Registry Noise key does not match the local identity"
                        .into(),
                    source: None,
                })?;
        }
        Ok(())
    }
}
