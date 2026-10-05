use super::*;
use crate::storage::PublicEndpointRecord;

struct PublicEndpointSyncResult {
    report: EndpointSyncReport,
    lease_released: bool,
}

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
        let (session_access, _, publication) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                let lease = self.claim_paykit_app_operation_in_transaction(tx, false)?;
                crate::storage::require_paykit_app_active(tx, &self.config.app_id)?;
                Ok((lease, tx.public_endpoint_records()))
            })
            .await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let (lease, records) = publication.expect("active session loads App publication state");
        let result = async {
            let details = self.payment.current_public_receiving_details().await?;
            self.sync_public_endpoints_with_lease(details, records, &lease, &session_access)
                .await
        }
        .await;
        self.finish_public_endpoint_sync(lease, result).await
    }

    /// Publish explicit public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints_with_receiving_details(
        &self,
        details: Vec<PublicReceivingDetail>,
    ) -> Result<EndpointSyncReport> {
        let _identity_guard = self.claim_identity_operation("sync public endpoints")?;
        let (session_access, _, publication) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                let lease = self.claim_paykit_app_operation_in_transaction(tx, false)?;
                crate::storage::require_paykit_app_active(tx, &self.config.app_id)?;
                Ok((lease, tx.public_endpoint_records()))
            })
            .await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let (lease, records) = publication.expect("active session loads App publication state");
        let result = self
            .sync_public_endpoints_with_lease(details, records, &lease, &session_access)
            .await;
        self.finish_public_endpoint_sync(lease, result).await
    }

    async fn finish_public_endpoint_sync(
        &self,
        lease: PaykitAppOperationLease,
        result: Result<PublicEndpointSyncResult>,
    ) -> Result<EndpointSyncReport> {
        match result {
            Ok(PublicEndpointSyncResult {
                report,
                lease_released: true,
            }) => Ok(report),
            result => {
                self.finish_paykit_app_operation(lease, result.map(|result| result.report))
                    .await
            }
        }
    }

    async fn sync_public_endpoints_with_lease(
        &self,
        details: Vec<PublicReceivingDetail>,
        records: Vec<PublicEndpointRecord>,
        lease: &PaykitAppOperationLease,
        session_access: &GuardedSessionAccess,
    ) -> Result<PublicEndpointSyncResult> {
        validate_public_endpoint_count(details.len())?;
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
        let mut current_identifiers = paykit_lib::list_payment_endpoint_identifiers(
            &session_access.outbox_client.public_storage(),
            session_access.session.info().public_key(),
            &self.config.app_id,
        )
        .await?
        .into_iter()
        .collect::<HashSet<_>>();

        let mut already_removed = Vec::new();
        let mut failed_removals = Vec::new();
        let removal_candidates = match self.config.endpoint_management_scope {
            EndpointManagementScope::ManagedOnly => {
                let records = records
                    .into_iter()
                    .filter(|record| {
                        record.app_id == self.config.app_id
                            && record.status != PublicationStatus::Removed
                            && !desired
                                .keys()
                                .any(|identifier| identifier.as_str() == record.identifier)
                    })
                    .collect::<Vec<_>>();
                let mut candidates = Vec::new();
                for record in records {
                    let identifier =
                        paykit_lib::PaymentEndpointIdentifier::new(&record.identifier)?;
                    // A publication record can contain an attempted, unpublished payload.
                    // Retain the current revision, including for empty endpoint files.
                    match paykit_lib::fetch_payment_endpoint_revision(
                        &session_access.session,
                        &self.config.app_id,
                        &identifier,
                    )
                    .await
                    {
                        Ok(Some(revision)) => {
                            candidates.push((record.identifier, record.payload, revision))
                        }
                        Ok(None) => already_removed.push(record),
                        Err(err) => failed_removals.push(failed_record(
                            &self.config.app_id,
                            record.identifier,
                            record.payload,
                            err.to_string(),
                            now,
                        )),
                    }
                }
                already_removed.sort_by(|left, right| left.identifier.cmp(&right.identifier));
                candidates
            }
            EndpointManagementScope::FullAppEndpointNamespace => {
                let remote_identifiers = current_identifiers
                    .iter()
                    .map(|identifier| identifier.as_str().to_owned())
                    .collect::<HashSet<_>>();
                already_removed = records
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
                let mut candidates = Vec::new();
                for identifier in &current_identifiers {
                    if desired.contains_key(identifier) {
                        continue;
                    }
                    match paykit_lib::fetch_payment_endpoint_revision(
                        &session_access.session,
                        &self.config.app_id,
                        identifier,
                    )
                    .await
                    {
                        Ok(Some(revision)) => {
                            candidates.push((identifier.as_str().to_owned(), None, revision))
                        }
                        Ok(None) => already_removed.push(removed_record(
                            &self.config.app_id,
                            identifier.as_str().to_owned(),
                            now,
                        )),
                        Err(error) => failed_removals.push(failed_record(
                            &self.config.app_id,
                            identifier.as_str().to_owned(),
                            None,
                            error.to_string(),
                            now,
                        )),
                    }
                }
                candidates
            }
        };

        let mut removal_candidates = removal_candidates;
        removal_candidates.sort_by(|(left, _, _), (right, _, _)| left.cmp(right));
        failed_removals.sort_by(|left, right| left.identifier.cmp(&right.identifier));
        self.retry_storage_transaction(|| {
            let app_id = self.config.app_id.clone();
            let lease = lease.clone();
            let desired_entries = desired_entries
                .iter()
                .map(|(identifier, payload)| ((*identifier).clone(), (*payload).clone()))
                .collect::<Vec<_>>();
            let removal_candidates = removal_candidates.clone();
            let already_removed = already_removed.clone();
            let failed_removals = failed_removals.clone();
            move |tx| {
                crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
                crate::storage::require_paykit_app_active(tx, &app_id)?;
                let records = tx.public_endpoint_records();
                for (identifier, payload) in desired_entries {
                    if records.iter().any(|record| {
                        *record
                            == published_record(&app_id, &identifier, &payload, record.updated_at)
                    }) {
                        continue;
                    }
                    tx.save_public_endpoint_record(pending_publication_record(
                        &app_id,
                        &identifier,
                        &payload,
                        now,
                    ));
                }
                for (identifier, previous_payload, _) in removal_candidates {
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
                for record in failed_removals {
                    tx.save_public_endpoint_record(record);
                }
                Ok(())
            }
        })
        .await?;

        for record in &already_removed {
            current_identifiers.remove(&paykit_lib::PaymentEndpointIdentifier::new(
                &record.identifier,
            )?);
        }
        report.removed.extend(
            already_removed
                .into_iter()
                .map(|record| EndpointSyncChange {
                    identifier: record.identifier,
                    status: PublicationStatus::Removed,
                    error: None,
                }),
        );
        report.failed.extend(
            failed_removals
                .into_iter()
                .map(|record| EndpointSyncChange {
                    identifier: record.identifier,
                    status: record.status,
                    error: record.last_error,
                }),
        );

        // Remove stale endpoints first so replacement at the limit stays discoverable.
        let mut lease_released = false;
        let mut removal_candidates = removal_candidates.into_iter().peekable();
        while let Some((identifier_text, previous_payload, revision)) = removal_candidates.next() {
            let completes_removal = removal_candidates.peek().is_none()
                && desired_entries.is_empty()
                && report.failed.is_empty();
            let identifier = paykit_lib::PaymentEndpointIdentifier::new(&identifier_text)?;
            self.with_guarded_storage_operation(
                Arc::clone(&session_access._guard),
                Box::pin(async {
                    self.require_paykit_app_operation_lease(lease).await?;
                    let result = self
                        .remove_public_endpoint_at_revision(session_access, &identifier, &revision)
                        .await;
                    match result {
                        Ok(()) => {
                            current_identifiers.remove(&identifier);
                            self.retry_storage_transaction(|| {
                                let lease = lease.clone();
                                let record = removed_record(
                                    &self.config.app_id,
                                    identifier_text.clone(),
                                    now,
                                );
                                move |tx| {
                                    crate::storage::require_paykit_app_operation_lease(tx, &lease)?;
                                    tx.save_public_endpoint_record(record);
                                    if completes_removal {
                                        tx.release_paykit_app_operation(
                                            &lease.app_id,
                                            lease.lease_id,
                                        );
                                    }
                                    Ok(())
                                }
                            })
                            .await?;
                            lease_released = completes_removal;
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
                    Ok(())
                }),
            )
            .await?;
        }

        for (identifier, payload) in desired_entries {
            let change = self
                .with_guarded_storage_operation(
                    Arc::clone(&session_access._guard),
                    Box::pin(async {
                        self.require_paykit_app_operation_lease(lease).await?;
                        let result = if !current_identifiers.contains(identifier) {
                            validate_public_endpoint_count(current_identifiers.len() + 1)
                        } else {
                            Ok(())
                        };
                        let result = match result {
                            Ok(()) => {
                                // A failed PUT may still have committed; reserve its slot for this sync.
                                current_identifiers.insert(identifier.clone());
                                self.publish_public_endpoint_if_current(
                                    session_access,
                                    identifier,
                                    payload,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        };
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
                                if change.status == PublicationStatus::Published
                                    && tx.public_endpoint_records().iter().any(|record| {
                                        *record
                                            == published_record(
                                                &app_id,
                                                &identifier,
                                                &payload,
                                                record.updated_at,
                                            )
                                    })
                                {
                                    return Ok(());
                                }
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
                        Ok(change)
                    }),
                )
                .await?;
            if change.status == PublicationStatus::Published {
                report.published.push(change);
            } else {
                report.failed.push(change);
            }
        }

        Ok(PublicEndpointSyncResult {
            report,
            lease_released,
        })
    }

    async fn publish_public_endpoint_if_current(
        &self,
        session_access: &PubkySessionAccess,
        identifier: &paykit_lib::PaymentEndpointIdentifier,
        payload: &paykit_lib::PaymentEndpointPayload,
    ) -> Result<()> {
        let current = paykit_lib::fetch_payment_endpoint_revision(
            &session_access.session,
            &self.config.app_id,
            identifier,
        )
        .await?;
        if current.as_deref()
            == Some(paykit_lib::content_revision(payload.as_str().as_bytes()).as_str())
        {
            return Ok(());
        }
        let write = match current {
            Some(revision) => {
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
        write.map_err(Into::into)
    }

    pub(super) async fn remove_public_endpoint_if_current(
        &self,
        session_access: &PubkySessionAccess,
        identifier: &paykit_lib::PaymentEndpointIdentifier,
        expected_payload: Option<&str>,
        lease: &PaykitAppOperationLease,
    ) -> Result<()> {
        let Some(revision) = paykit_lib::fetch_payment_endpoint_revision(
            &session_access.session,
            &self.config.app_id,
            identifier,
        )
        .await?
        else {
            return Ok(());
        };
        if expected_payload
            .is_some_and(|payload| paykit_lib::content_revision(payload.as_bytes()) != revision)
        {
            return Err(PaykitSdkError::ConcurrentUpdate {
                context: format!(
                    "public Payment Endpoint '{}' changed before removal",
                    identifier.as_str()
                ),
                source: None,
            });
        }
        self.require_paykit_app_operation_lease(lease).await?;
        self.remove_public_endpoint_at_revision(session_access, identifier, &revision)
            .await
    }

    async fn remove_public_endpoint_at_revision(
        &self,
        session_access: &PubkySessionAccess,
        identifier: &paykit_lib::PaymentEndpointIdentifier,
        revision: &str,
    ) -> Result<()> {
        match paykit_lib::remove_payment_endpoint_if_revision(
            &session_access.session,
            &self.config.app_id,
            identifier.clone(),
            revision,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(err) if paykit_lib::is_write_conflict(&err) => {
                if paykit_lib::fetch_payment_endpoint_revision(
                    &session_access.session,
                    &self.config.app_id,
                    identifier,
                )
                .await?
                .is_none()
                {
                    Ok(())
                } else {
                    Err(err.into())
                }
            }
            Err(err) => Err(err.into()),
        }
    }
}

fn validate_public_endpoint_count(count: usize) -> Result<()> {
    if count > paykit_lib::PAYMENT_LIST_MAX_ENDPOINTS {
        return Err(paykit_lib::PaykitError::Validation(format!(
            "Payment List must not exceed {} endpoints",
            paykit_lib::PAYMENT_LIST_MAX_ENDPOINTS
        ))
        .into());
    }
    Ok(())
}
