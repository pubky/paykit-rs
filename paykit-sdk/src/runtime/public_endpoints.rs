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
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let lease = self.claim_paykit_app_operation().await?;
        let result = async {
            let details = self.payment.current_public_receiving_details().await?;
            self.sync_public_endpoints_with_lease(details, &lease, &session_access)
                .await
        }
        .await;
        self.finish_paykit_app_operation(lease, result).await
    }

    /// Publish explicit public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints_with_receiving_details(
        &self,
        details: Vec<PublicReceivingDetail>,
    ) -> Result<EndpointSyncReport> {
        let _identity_guard = self.claim_identity_operation("sync public endpoints")?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let lease = self.claim_paykit_app_operation().await?;
        let result = self
            .sync_public_endpoints_with_lease(details, &lease, &session_access)
            .await;
        self.finish_paykit_app_operation(lease, result).await
    }

    async fn sync_public_endpoints_with_lease(
        &self,
        details: Vec<PublicReceivingDetail>,
        lease: &PaykitAppOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<EndpointSyncReport> {
        self.retry_storage_transaction(|| {
            let app_id = self.config.app_id.clone();
            let lease = lease.clone();
            move |tx| {
                crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
                crate::storage::require_paykit_app_active(tx, &app_id)
            }
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

        let mut already_removed = Vec::new();
        let removal_candidates = match self.config.endpoint_management_scope {
            EndpointManagementScope::ManagedOnly => self
                .retry_storage_transaction(|| |tx| Ok(tx.public_endpoint_records()))
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
                already_removed = self
                    .retry_storage_transaction(|| |tx| Ok(tx.public_endpoint_records()))
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
                already_removed.sort_by(|left, right| left.identifier.cmp(&right.identifier));
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
        self.retry_storage_transaction(|| {
            let app_id = self.config.app_id.clone();
            let lease = lease.clone();
            let desired_entries = desired_entries
                .iter()
                .map(|(identifier, payload)| ((*identifier).clone(), (*payload).clone()))
                .collect::<Vec<_>>();
            let removal_candidates = removal_candidates.clone();
            let already_removed = already_removed.clone();
            move |tx| {
                crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
                crate::storage::require_paykit_app_active(tx, &app_id)?;
                for (identifier, payload) in desired_entries {
                    tx.save_public_endpoint_record(pending_publication_record(
                        &app_id,
                        &identifier,
                        &payload,
                        now,
                    ));
                }
                for (identifier, previous_payload) in removal_candidates {
                    tx.save_public_endpoint_record(pending_removal_record(
                        &app_id,
                        identifier,
                        previous_payload,
                        now,
                    ));
                }
                for record in already_removed {
                    tx.save_public_endpoint_record(removed_record(&app_id, record.identifier, now));
                }
                Ok(())
            }
        })
        .await?;

        report.removed.extend(
            already_removed
                .into_iter()
                .map(|record| EndpointSyncChange {
                    identifier: record.identifier,
                    status: PublicationStatus::Removed,
                    error: None,
                }),
        );

        for (identifier, payload) in desired_entries {
            self.require_paykit_app_operation_lease(lease).await?;
            let result = self
                .publish_public_endpoint_if_current(session_access, identifier, payload)
                .await;
            let change = match result {
                Ok(()) => EndpointSyncChange {
                    identifier: identifier.as_str().to_owned(),
                    status: PublicationStatus::Published,
                    error: None,
                },
                Err(err) => EndpointSyncChange {
                    identifier: identifier.as_str().to_owned(),
                    status: PublicationStatus::Failed,
                    error: Some(err.to_string()),
                },
            };
            self.retry_storage_transaction(|| {
                let app_id = self.config.app_id.clone();
                let lease = lease.clone();
                let identifier = identifier.clone();
                let payload = payload.clone();
                let change = change.clone();
                move |tx| {
                    crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
                    let record = if change.status == PublicationStatus::Published {
                        published_record(&app_id, &identifier, &payload, now)
                    } else {
                        failed_record(
                            &app_id,
                            identifier.as_str().to_owned(),
                            Some(payload.as_str().to_owned()),
                            change
                                .error
                                .clone()
                                .expect("failed publication has an error"),
                            now,
                        )
                    };
                    tx.save_public_endpoint_record(record);
                    Ok(())
                }
            })
            .await?;
            if change.status == PublicationStatus::Published {
                report.published.push(change);
            } else {
                report.failed.push(change);
            }
        }

        for (identifier_text, previous_payload) in removal_candidates {
            let identifier = paykit_lib::PaymentEndpointIdentifier::new(&identifier_text)?;
            self.require_paykit_app_operation_lease(lease).await?;
            match self
                .remove_public_endpoint_if_current(
                    session_access,
                    &identifier,
                    previous_payload.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    self.retry_storage_transaction(|| {
                        let lease = lease.clone();
                        let record =
                            removed_record(&self.config.app_id, identifier_text.clone(), now);
                        move |tx| {
                            crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
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
                    self.retry_storage_transaction(|| {
                        let lease = lease.clone();
                        let record = failed_record(
                            &self.config.app_id,
                            identifier_text.clone(),
                            previous_payload.clone(),
                            error.clone(),
                            now,
                        );
                        move |tx| {
                            crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
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

    async fn publish_public_endpoint_if_current(
        &self,
        session_access: &PubkySessionAccess,
        identifier: &paykit_lib::PaymentEndpointIdentifier,
        payload: &paykit_lib::PaymentEndpointPayload,
    ) -> Result<()> {
        let session_info = session_access.session.info();
        let owner = session_info.public_key();
        let current = paykit_lib::get_payment_endpoint_with_revision(
            &session_access.outbox_client.public_storage(),
            owner,
            &self.config.app_id,
            identifier,
        )
        .await?;
        if current.as_ref().and_then(|(payload, _)| payload.as_ref()) == Some(payload) {
            return Ok(());
        }
        let write = match current {
            Some((_, revision)) => {
                paykit_lib::update_payment_endpoint(
                    &session_access.session,
                    &self.config.app_id,
                    identifier.clone(),
                    payload.clone(),
                    &revision,
                )
                .await
            }
            None => {
                paykit_lib::create_payment_endpoint(
                    &session_access.session,
                    &self.config.app_id,
                    identifier.clone(),
                    payload.clone(),
                )
                .await
            }
        };
        write.map_err(map_endpoint_conditional_error)
    }

    pub(super) async fn remove_public_endpoint_if_current(
        &self,
        session_access: &PubkySessionAccess,
        identifier: &paykit_lib::PaymentEndpointIdentifier,
        expected_payload: Option<&str>,
    ) -> Result<()> {
        let session_info = session_access.session.info();
        let owner = session_info.public_key();
        let public_storage = session_access.outbox_client.public_storage();
        let Some((current_payload, revision)) = paykit_lib::get_payment_endpoint_with_revision(
            &public_storage,
            owner,
            &self.config.app_id,
            identifier,
        )
        .await?
        else {
            return Ok(());
        };
        if expected_payload.is_some()
            && current_payload.as_ref().map(|payload| payload.as_str()) != expected_payload
        {
            return Err(PaykitSdkError::ConcurrentUpdate {
                context: format!(
                    "public Payment Endpoint '{}' changed before removal",
                    identifier.as_str()
                ),
                source: None,
            });
        }
        match paykit_lib::remove_payment_endpoint_if_revision(
            &session_access.session,
            &self.config.app_id,
            identifier.clone(),
            &revision,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(err) if paykit_error_is_precondition_failed(&err) => {
                if paykit_lib::get_payment_endpoint_with_revision(
                    &public_storage,
                    owner,
                    &self.config.app_id,
                    identifier,
                )
                .await?
                .is_none()
                {
                    Ok(())
                } else {
                    Err(map_endpoint_conditional_error(err))
                }
            }
            Err(err) => Err(err.into()),
        }
    }
}

fn map_endpoint_conditional_error(error: paykit_lib::PaykitError) -> PaykitSdkError {
    if paykit_error_is_precondition_failed(&error) {
        PaykitSdkError::ConcurrentUpdate {
            context: "public Payment Endpoint changed during synchronization".into(),
            source: Some(error.into()),
        }
    } else {
        error.into()
    }
}

fn paykit_error_is_precondition_failed(error: &paykit_lib::PaykitError) -> bool {
    matches!(
        error,
        paykit_lib::PaykitError::Transport { source, .. }
            if source
                .downcast_ref::<PubkyError>()
                .is_some_and(is_pubky_precondition_failed)
    )
}
