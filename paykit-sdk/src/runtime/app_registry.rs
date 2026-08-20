use super::app_removal::{
    app_removal_blockers, begin_paykit_app_removal, detach_shared_app_reservations,
    reactivate_paykit_app, require_app_capability_downgrade_safe,
    retire_app_outbound_private_messages,
};
use super::*;

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
            let mut private_apps = Vec::new();
            let mut payment_request_apps = Vec::new();
            let mut receipt_apps = Vec::new();
            if let Some(registry) = registry.as_ref() {
                for (app_id, app) in registry.apps() {
                    let capabilities = app.capabilities();
                    if capabilities.private_payments {
                        private_apps.push(app_id.clone());
                    }
                    if capabilities.payment_requests {
                        payment_request_apps.push(app_id.clone());
                    }
                    if capabilities.receipts {
                        receipt_apps.push(app_id.clone());
                    }
                }
            }
            private_apps.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            payment_request_apps.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            receipt_apps.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            self.storage
                .transaction({
                    let counterparty = counterparty.clone();
                    let private_apps = private_apps.clone();
                    let payment_request_apps = payment_request_apps.clone();
                    let receipt_apps = receipt_apps.clone();
                    move |tx| {
                        tx.save_authorized_private_apps(counterparty.clone(), private_apps);
                        tx.save_authorized_payment_request_apps(
                            counterparty.clone(),
                            payment_request_apps,
                        );
                        tx.save_authorized_receipt_apps(counterparty, receipt_apps);
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
            let (private_apps, payment_request_apps, receipt_apps) = self
                .storage
                .transaction(|tx| {
                    Ok((
                        tx.authorized_private_apps(counterparty),
                        tx.authorized_payment_request_apps(counterparty),
                        tx.authorized_receipt_apps(counterparty),
                    ))
                })
                .await?;
            Ok(CounterpartyAppAuthorizationContext {
                registry: None,
                private_apps,
                payment_request_apps,
                receipt_apps,
            })
        }
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
    /// not complete.
    ///
    /// Calls that mutate the same identity's registry must be serialized
    /// across SDK instances until Pubky supports conditional registry writes.
    pub async fn publish_paykit_app(
        &self,
        app: paykit_lib::PaykitApp,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        let _identity_guard = self.claim_identity_operation("publish Paykit app")?;
        let app_id = self.config.app_id.clone();
        let capabilities = app.capabilities();
        let (session_access, mut registry) = self
            .paykit_app_registry_update_context("publish Paykit app", true)
            .await?;
        if let Some(previous) = registry.apps().get(&app_id) {
            require_app_capability_downgrade_safe(
                &self.storage,
                &app_id,
                previous.capabilities(),
                capabilities,
                self.clock.now(),
            )
            .await?;
        }
        registry.register_app(app_id.clone(), app)?;
        paykit_lib::set_paykit_app_registry(&session_access.session, &registry).await?;
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
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
    ///
    /// Calls that mutate the same identity's registry must be serialized
    /// across SDK instances until Pubky supports conditional registry writes.
    pub async fn remove_paykit_app(&self) -> Result<paykit_lib::PaykitAppRegistry> {
        let _identity_guard = self.claim_identity_operation("remove Paykit app")?;
        let (session_access, mut registry) = self
            .paykit_app_registry_update_context("remove Paykit app", true)
            .await?;
        let app_id = self.config.app_id.clone();
        let registry_capabilities = registry.apps().get(&app_id).map(|app| app.capabilities());
        let registry_entry_exists = registry_capabilities.is_some();
        let was_locally_active = begin_paykit_app_removal(&self.storage, &app_id).await?;
        let blockers = match app_removal_blockers(&self.storage, &app_id, self.clock.now()).await {
            Ok(blockers) => blockers,
            Err(err) => {
                if was_locally_active {
                    reactivate_paykit_app(&self.storage, app_id).await?;
                }
                return Err(err);
            }
        };
        if !blockers.is_empty() {
            if was_locally_active {
                reactivate_paykit_app(&self.storage, app_id).await?;
            }
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
        let now = self.clock.now();
        let lease_timeout = ChronoDuration::from_std(PEER_LINK_OPERATION_LEASE_TIMEOUT)
            .expect("fixed peer link lease timeout must fit chrono duration");
        let leases = self
            .storage
            .transaction({
                let app_id = app_id.clone();
                move |tx| {
                    retire_app_outbound_private_messages(tx, &app_id, now, now + lease_timeout)
                }
            })
            .await?;
        let result = async {
            let mut cleanup_failures = Vec::new();
            for lease in &leases {
                cleanup_failures.extend(
                    self.cancel_terminal_private_list_reservations(
                        &lease.counterparty,
                        Some(lease),
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
                    let app_id = app_id.clone();
                    move |tx| detach_shared_app_reservations(tx, &app_id)
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

            let payment_list = paykit_lib::get_payment_list(
                &session_access.outbox_client.public_storage(),
                session_access.session.info().public_key(),
                &app_id,
            )
            .await?;
            let mut identifiers = payment_list
                .payment_endpoints
                .into_keys()
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
                paykit_lib::remove_payment_endpoint(&session_access.session, &app_id, identifier)
                    .await?;
            }

            self.storage
                .transaction({
                    let app_id = app_id.clone();
                    move |tx| {
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

            if registry_entry_exists {
                registry.remove_app(&app_id);
                paykit_lib::set_paykit_app_registry(&session_access.session, &registry).await?;
            }
            Ok(registry)
        }
        .await;

        let mut release_error = None;
        for lease in &leases {
            if let Err(err) = self.release_peer_link_operation(lease).await {
                release_error.get_or_insert(err);
            }
        }
        match (result, release_error) {
            (Ok(registry), None) => Ok(registry),
            (Err(err), _) => Err(err),
            (Ok(_), Some(err)) => Err(err),
        }
    }

    /// Report work that must finish before this application can be removed.
    pub async fn paykit_app_removal_blockers(&self) -> Result<PaykitAppRemovalBlockers> {
        app_removal_blockers(&self.storage, &self.config.app_id, self.clock.now()).await
    }

    /// Set or clear the identity-wide default Paykit application.
    ///
    /// Calls that mutate the same identity's registry must be serialized
    /// across SDK instances until Pubky supports conditional registry writes.
    pub async fn set_default_paykit_app(
        &self,
        app_id: Option<paykit_lib::PaykitAppId>,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        self.update_paykit_app_registry("set default Paykit app", false, move |registry| {
            registry.set_default_app(app_id)?;
            Ok(())
        })
        .await
    }

    /// Set or clear the default Paykit application for one endpoint identifier.
    ///
    /// Calls that mutate the same identity's registry must be serialized
    /// across SDK instances until Pubky supports conditional registry writes.
    pub async fn set_default_paykit_app_for_endpoint(
        &self,
        identifier: paykit_lib::PaymentEndpointIdentifier,
        app_id: Option<paykit_lib::PaykitAppId>,
    ) -> Result<paykit_lib::PaykitAppRegistry> {
        self.update_paykit_app_registry(
            "set default Paykit app for endpoint",
            false,
            move |registry| {
                if let Some(app_id) = app_id {
                    registry.set_default_app_for_endpoint(identifier, app_id)?;
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
        F: FnOnce(&mut paykit_lib::PaykitAppRegistry) -> Result<()>,
    {
        let _identity_guard = self.claim_identity_operation(operation)?;
        let (session_access, mut registry) = self
            .paykit_app_registry_update_context(operation, create_if_missing)
            .await?;
        update(&mut registry)?;
        paykit_lib::set_paykit_app_registry(&session_access.session, &registry).await?;
        Ok(registry)
    }

    async fn paykit_app_registry_update_context(
        &self,
        operation: &'static str,
        create_if_missing: bool,
    ) -> Result<(PubkySessionAccess, paykit_lib::PaykitAppRegistry)> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let local_noise_public_key = session_access.local_secret_key.as_ref().map(|secret_key| {
            let noise_secret_key = secret_key.paykit_noise_secret_key();
            pubky::Keypair::from_secret(&noise_secret_key)
                .public_key()
                .clone()
        });
        let public_storage = session_access.outbox_client.public_storage();
        let owner = session_access.session.info().public_key();
        let existing = paykit_lib::get_paykit_app_registry(&public_storage, owner).await?;
        let registry = match existing {
            Some(registry) => registry,
            None if create_if_missing => paykit_lib::PaykitAppRegistry::new(
                local_noise_public_key
                    .clone()
                    .ok_or_else(|| PaykitSdkError::Identity {
                        context: format!(
                            "local Pubky secret key is unavailable to create the Paykit App Registry during {operation}"
                        ),
                        source: None,
                    })?,
            ),
            None => {
                return Err(PaykitSdkError::NotFound {
                    context: "Paykit app registry".into(),
                    source: None,
                });
            }
        };
        if let Some(local_noise_public_key) = local_noise_public_key {
            if registry.noise_public_key() != &local_noise_public_key {
                return Err(PaykitSdkError::Identity {
                    context: "Paykit App Registry Noise key does not match the local identity"
                        .into(),
                    source: None,
                });
            }
        }
        Ok((session_access, registry))
    }
}
