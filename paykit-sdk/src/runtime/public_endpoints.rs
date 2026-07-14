use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// List public Paykit receiver paths published by a Pubky identity.
    ///
    /// This is a discovery helper. Callers still choose the exact receiver path
    /// they want to use for public/private payment workflows.
    pub async fn paykit_receiver_paths(
        &self,
        owner: PubkyPublicKey,
    ) -> Result<Vec<PaykitReceiverPath>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available".into(),
                    source: None,
                })?;
        Ok(
            paykit_lib::list_paykit_receiver_paths(&public_storage, &owner.to_public_key()?)
                .await?,
        )
    }

    /// Fetch one public Paykit receiver marker, if present.
    pub async fn paykit_receiver_marker(
        &self,
        owner: PubkyPublicKey,
        receiver_path: PaykitReceiverPath,
    ) -> Result<Option<PaykitReceiverMarker>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available".into(),
                    source: None,
                })?;
        Ok(paykit_lib::get_paykit_receiver_marker(
            &public_storage,
            &owner.to_public_key()?,
            &receiver_path,
        )
        .await?)
    }

    /// Publish the configured local receiver marker.
    pub async fn publish_paykit_receiver_marker(
        &self,
        capabilities: PaykitReceiverCapabilities,
    ) -> Result<PaykitReceiverMarker> {
        let _identity_guard = self.claim_identity_operation("publish Paykit receiver marker")?;
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        validate_receiver_marker_capabilities(&capabilities, identity.capability)?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let marker = PaykitReceiverMarker::new(self.config.receiver_path.clone(), capabilities);
        paykit_lib::publish_paykit_receiver_marker(&session_access.session, &marker).await?;
        Ok(marker)
    }

    /// Remove the configured local receiver marker.
    pub async fn remove_paykit_receiver_marker(&self) -> Result<()> {
        let _identity_guard = self.claim_identity_operation("remove Paykit receiver marker")?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        paykit_lib::remove_paykit_receiver_marker(
            &session_access.session,
            &self.config.receiver_path,
        )
        .await?;
        Ok(())
    }

    /// Publish current public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints(&self) -> Result<EndpointSyncReport> {
        let details = self
            .payment
            .current_receiving_details(ReceivingDetailScope::Public)
            .await?;
        self.sync_public_endpoints_with_receiving_details(details)
            .await
    }

    /// Publish explicit public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints_with_receiving_details(
        &self,
        details: Vec<ReceivingDetail>,
    ) -> Result<EndpointSyncReport> {
        let _identity_guard = self.claim_identity_operation("sync public endpoints")?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let desired = normalize_receiving_details(details)?;
        let now = self.clock.now();
        let mut report = EndpointSyncReport::default();
        let mut desired_entries = desired.iter().collect::<Vec<_>>();
        desired_entries.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));

        for (identifier, payload) in desired_entries {
            self.storage
                .transaction({
                    let record = pending_publication_record(identifier, payload, now);
                    move |tx| {
                        tx.save_public_endpoint_record(record);
                        Ok(())
                    }
                })
                .await?;
            match paykit_lib::set_payment_endpoint(
                &session_access.session,
                &self.config.receiver_path,
                identifier.clone(),
                payload.clone(),
            )
            .await
            {
                Ok(()) => {
                    self.storage
                        .transaction({
                            let record = published_record(identifier, payload, now);
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.published.push(EndpointSyncChange {
                        identifier: identifier.as_str().to_owned(),
                        status: PublicationStatus::Published,
                        error: None,
                    });
                }
                Err(err) => {
                    let error = err.to_string();
                    self.storage
                        .transaction({
                            let record = failed_record(
                                identifier.as_str().to_owned(),
                                Some(payload.as_str().to_owned()),
                                error.clone(),
                                now,
                            );
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(EndpointSyncChange {
                        identifier: identifier.as_str().to_owned(),
                        status: PublicationStatus::Failed,
                        error: Some(error),
                    });
                }
            }
        }

        let removal_candidates = match self.config.endpoint_management_scope {
            EndpointManagementScope::ManagedOnly => self
                .storage
                .transaction(|tx| Ok(tx.public_endpoint_records()))
                .await?
                .into_iter()
                .filter(|record| {
                    record.status != PublicationStatus::Removed
                        && !desired
                            .keys()
                            .any(|identifier| identifier.as_str() == record.identifier)
                })
                .map(|record| (record.identifier, record.payload))
                .collect::<Vec<_>>(),
            EndpointManagementScope::FullPaykitNamespace => {
                let local_public_key = session_access.session.info().public_key().clone();
                let current = paykit_lib::get_payment_list(
                    &session_access.outbox_client.public_storage(),
                    &local_public_key,
                    &self.config.receiver_path,
                )
                .await?;
                let remote_identifiers = current
                    .payment_endpoints
                    .keys()
                    .map(|identifier| identifier.as_str().to_owned())
                    .collect::<HashSet<_>>();
                let already_removed = self
                    .storage
                    .transaction(|tx| Ok(tx.public_endpoint_records()))
                    .await?
                    .into_iter()
                    .filter(|record| {
                        matches!(
                            record.status,
                            PublicationStatus::PendingRemoval | PublicationStatus::Failed
                        ) && !remote_identifiers.contains(&record.identifier)
                            && !desired
                                .keys()
                                .any(|identifier| identifier.as_str() == record.identifier)
                    })
                    .collect::<Vec<_>>();
                let mut already_removed = already_removed;
                already_removed.sort_by(|left, right| left.identifier.cmp(&right.identifier));
                for record in already_removed {
                    self.storage
                        .transaction({
                            let removed = removed_record(record.identifier.clone(), now);
                            move |tx| {
                                tx.save_public_endpoint_record(removed);
                                Ok(())
                            }
                        })
                        .await?;
                    report.removed.push(EndpointSyncChange {
                        identifier: record.identifier,
                        status: PublicationStatus::Removed,
                        error: None,
                    });
                }
                current
                    .payment_endpoints
                    .into_iter()
                    .filter(|(identifier, _)| !desired.contains_key(identifier))
                    .map(|(identifier, payload)| {
                        (identifier.as_str().to_owned(), Some(payload.into_inner()))
                    })
                    .collect::<Vec<_>>()
            }
        };

        let mut removal_candidates = removal_candidates;
        removal_candidates.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (identifier_text, previous_payload) in removal_candidates {
            let identifier = paykit_lib::PaymentEndpointIdentifier::new(&identifier_text)?;
            self.storage
                .transaction({
                    let record = pending_removal_record(
                        identifier_text.clone(),
                        previous_payload.clone(),
                        now,
                    );
                    move |tx| {
                        tx.save_public_endpoint_record(record);
                        Ok(())
                    }
                })
                .await?;
            match paykit_lib::remove_payment_endpoint(
                &session_access.session,
                &self.config.receiver_path,
                identifier,
            )
            .await
            {
                Ok(()) => {
                    self.storage
                        .transaction({
                            let record = removed_record(identifier_text.clone(), now);
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.removed.push(EndpointSyncChange {
                        identifier: identifier_text,
                        status: PublicationStatus::Removed,
                        error: None,
                    });
                }
                Err(err) => {
                    let error = err.to_string();
                    self.storage
                        .transaction({
                            let record = failed_record(
                                identifier_text.clone(),
                                previous_payload,
                                error.clone(),
                                now,
                            );
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(EndpointSyncChange {
                        identifier: identifier_text,
                        status: PublicationStatus::Failed,
                        error: Some(error),
                    });
                }
            }
        }

        Ok(report)
    }
}

pub(super) fn validate_receiver_marker_capabilities(
    capabilities: &PaykitReceiverCapabilities,
    identity_capability: PubkyIdentityCapability,
) -> Result<()> {
    let advertises_private_workflows =
        capabilities.private_payments || capabilities.payment_requests || capabilities.receipts;
    if advertises_private_workflows
        && identity_capability != PubkyIdentityCapability::PrivateLinkCapable
    {
        return Err(PaykitSdkError::Policy(
            "receiver marker private capabilities require a local Pubky secret key".into(),
        ));
    }
    Ok(())
}
