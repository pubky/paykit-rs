use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Publish current public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints(&self) -> Result<EndpointSyncReport> {
        let _identity_guard = self.claim_identity_operation("sync public endpoints")?;
        let details = self.payment.current_public_receiving_details().await?;
        self.sync_public_endpoints_unguarded(details).await
    }

    /// Publish explicit public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints_with_receiving_details(
        &self,
        details: Vec<PublicReceivingDetail>,
    ) -> Result<EndpointSyncReport> {
        let _identity_guard = self.claim_identity_operation("sync public endpoints")?;
        self.sync_public_endpoints_unguarded(details).await
    }

    async fn sync_public_endpoints_unguarded(
        &self,
        details: Vec<PublicReceivingDetail>,
    ) -> Result<EndpointSyncReport> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        self.storage
            .transaction({
                let app_id = self.config.app_id.clone();
                move |tx| crate::storage::require_paykit_app_active(tx, &app_id)
            })
            .await?;
        let registry = paykit_lib::get_paykit_app_registry(
            &session_access.outbox_client.public_storage(),
            session_access.session.info().public_key(),
        )
        .await?
        .ok_or_else(|| PaykitSdkError::Policy {
            context: "publish the local Paykit app before syncing public Payment Endpoints".into(),
            source: None,
        })?;
        if !registry.apps().contains_key(&self.config.app_id) {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "Paykit app '{}' must be registered before syncing public Payment Endpoints",
                    self.config.app_id
                ),
                source: None,
            });
        }
        let desired = normalize_receiving_details(details)?;
        let now = self.clock.now();
        let mut report = EndpointSyncReport::default();
        let mut desired_entries = desired.iter().collect::<Vec<_>>();
        desired_entries.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));

        for (identifier, payload) in desired_entries {
            self.storage
                .transaction({
                    let record =
                        pending_publication_record(&self.config.app_id, identifier, payload, now);
                    move |tx| {
                        tx.save_public_endpoint_record(record);
                        Ok(())
                    }
                })
                .await?;
            match paykit_lib::set_payment_endpoint(
                &session_access.session,
                &self.config.app_id,
                identifier.clone(),
                payload.clone(),
            )
            .await
            {
                Ok(()) => {
                    self.storage
                        .transaction({
                            let record =
                                published_record(&self.config.app_id, identifier, payload, now);
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
                                &self.config.app_id,
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
                    record.app_id == self.config.app_id
                        && record.status != PublicationStatus::Removed
                        && !desired
                            .keys()
                            .any(|identifier| identifier.as_str() == record.identifier)
                })
                .map(|record| (record.identifier, record.payload))
                .collect::<Vec<_>>(),
            EndpointManagementScope::FullAppEndpointNamespace => {
                let local_public_key = session_access.session.info().public_key().clone();
                let current = paykit_lib::get_payment_list(
                    &session_access.outbox_client.public_storage(),
                    &local_public_key,
                    &self.config.app_id,
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
                        record.app_id == self.config.app_id
                            && matches!(
                                record.status,
                                PublicationStatus::PendingRemoval | PublicationStatus::Failed
                            )
                            && !remote_identifiers.contains(&record.identifier)
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
                            let removed =
                                removed_record(&self.config.app_id, record.identifier.clone(), now);
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
                        &self.config.app_id,
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
                &self.config.app_id,
                identifier,
            )
            .await
            {
                Ok(()) => {
                    self.storage
                        .transaction({
                            let record =
                                removed_record(&self.config.app_id, identifier_text.clone(), now);
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
                                &self.config.app_id,
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
