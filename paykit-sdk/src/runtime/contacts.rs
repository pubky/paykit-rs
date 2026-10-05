use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Save or update a Contact Record.
    pub async fn save_contact(&self, update: ContactUpdate) -> Result<ContactRecord> {
        update.validate()?;
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let local_public_key = initialized_identity_in_transaction(tx, "save contact")?;
                if update.public_key == local_public_key {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot save the local Paykit identity as a contact".into(),
                        source: None,
                    });
                }
                Ok(save_contact_in_transaction(tx, update, now))
            })
            .await
    }

    /// Save or update Contact Records in one atomic storage transaction.
    ///
    /// All updates and the initialized identity are checked before any record
    /// changes. Records are returned in input order. Duplicate keys are applied
    /// in that order, so the last update wins in storage; each returned record
    /// reflects its corresponding update. All records use one operation timestamp.
    /// An empty batch still requires an initialized identity and leaves stored
    /// state unchanged.
    ///
    /// Existing profile and Public Contact Marker metadata is preserved. This
    /// does not publish markers or unblock peers.
    pub async fn save_contacts(&self, updates: Vec<ContactUpdate>) -> Result<Vec<ContactRecord>> {
        for update in &updates {
            update.validate()?;
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let local_public_key = initialized_identity_in_transaction(tx, "save contacts")?;
                if updates
                    .iter()
                    .any(|update| update.public_key == local_public_key)
                {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot save the local Paykit identity as a contact".into(),
                        source: None,
                    });
                }
                Ok(updates
                    .into_iter()
                    .map(|update| save_contact_in_transaction(tx, update, now))
                    .collect())
            })
            .await
    }

    /// Load one Contact Record.
    pub async fn contact_record(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.storage
            .transaction(|tx| {
                initialized_identity_in_transaction(tx, "load contact")?;
                Ok(tx.contact_record(public_key))
            })
            .await
    }

    /// List Contact Records.
    pub async fn contact_records(&self) -> Result<Vec<ContactRecord>> {
        self.storage
            .transaction(|tx| {
                initialized_identity_in_transaction(tx, "list contacts")?;
                Ok(tx.contact_records())
            })
            .await
    }

    /// Remove one Contact Record.
    pub async fn remove_contact(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.storage
            .transaction(|tx| {
                initialized_identity_in_transaction(tx, "remove contact")?;
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(None);
                };
                if !existing.can_remove_locally() {
                    return Err(PaykitSdkError::Policy {
                        context: format!(
                            "remove public contact marker before deleting contact {public_key}"
                        ),
                        source: None,
                    });
                }
                Ok(tx.remove_contact_record(public_key))
            })
            .await
    }

    /// Fetch a contact's public profile and cache it in the Contact Record.
    pub async fn refresh_contact_paykit_profile(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.require_initialized_identity("refresh contact Paykit profile")
            .await?;
        let fetched = self.fetch_paykit_profile(public_key.clone()).await?;
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Ok(None);
                };
                let record = existing.with_profile(fetched.map(|record| record.profile), now);
                tx.save_contact_record(record.clone());
                Ok(Some(record))
            })
            .await
    }

    /// Publish a public contact marker for a saved contact.
    ///
    /// This can reveal part of the identity's contact graph. It only runs when
    /// `public_contact_sharing` is `Enabled`. Publication, removal, and retries
    /// share a homeserver lock for this contact's marker.
    pub async fn publish_public_contact(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<ContactRecord> {
        if self.config.public_contact_sharing != PublicContactSharingPolicy::Enabled {
            return Err(PaykitSdkError::Policy {
                context: "public contact sharing is disabled".into(),
                source: None,
            });
        }
        let session_access = self
            .load_session_access_for_initialized_identity("publish public contact")
            .await?;
        paykit_lib::with_write_lock(
            &session_access.session,
            &public_contact_path(&public_key),
            |lock| self.publish_public_contact_locked(&session_access.session, lock, &public_key),
        )
        .await
    }

    async fn publish_public_contact_locked(
        &self,
        session: &pubky::PubkySession,
        lock: pubky::StorageLock,
        public_key: &PubkyPublicKey,
    ) -> Result<ContactRecord> {
        let pending_at = self.clock.now();
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Err(PaykitSdkError::Protocol {
                        context: format!("cannot publish unsaved contact {public_key}"),
                        source: None,
                    });
                };
                tx.save_contact_record(
                    existing.mark_public_contact_publication_pending(pending_at),
                );
                Ok(())
            })
            .await?;
        let write_result = session
            .storage()
            .put_locked(&lock, public_contact_json(public_key)?)
            .await
            .map_err(|err| map_pubky_transport_error("publish public contact", err));
        if let Err(err) = write_result {
            self.mark_public_contact_failed(
                public_key,
                PublicationStatus::PendingPublication,
                err.to_string(),
            )
            .await?;
            return Err(err);
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "contact {public_key} disappeared before public publication was recorded"
                        ),
                        source: None,
                    });
                };
                require_pending_contact_marker(&existing, PublicationStatus::PendingPublication)?;
                let record = existing.mark_public_contact_published(now);
                tx.save_contact_record(record.clone());
                Ok(record)
            })
            .await
    }

    /// Remove a public contact marker for a saved contact.
    ///
    /// Cleanup is allowed even when public contact sharing is disabled, so an
    /// app can stop publishing markers and still remove previously published
    /// markers. The active session still needs write access to the Paykit
    /// public contact marker path.
    pub async fn remove_public_contact(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        let session_access = self
            .load_session_access_for_initialized_identity("remove public contact")
            .await?;
        paykit_lib::with_write_lock(
            &session_access.session,
            &public_contact_path(&public_key),
            |lock| self.remove_public_contact_locked(&session_access.session, lock, &public_key),
        )
        .await
    }

    async fn remove_public_contact_locked(
        &self,
        session: &pubky::PubkySession,
        lock: pubky::StorageLock,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        let pending_at = self.clock.now();
        let had_local_record = self
            .storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(false);
                };
                tx.save_contact_record(existing.mark_public_contact_removal_pending(pending_at));
                Ok(true)
            })
            .await?;
        let delete_result = session.storage().delete_locked(&lock).await;
        if let Err(err) = delete_result {
            if !is_pubky_not_found(&err) {
                let err = map_pubky_transport_error("remove public contact", err);
                self.mark_public_contact_failed(
                    public_key,
                    PublicationStatus::PendingRemoval,
                    err.to_string(),
                )
                .await?;
                return Err(err);
            }
        }
        if !had_local_record {
            return Ok(None);
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(None);
                };
                require_pending_contact_marker(&existing, PublicationStatus::PendingRemoval)?;
                let record = existing.mark_public_contact_removed(now);
                tx.save_contact_record(record.clone());
                Ok(Some(record))
            })
            .await
    }

    /// Retry pending public contact marker publication/removal records.
    pub async fn sync_public_contact_markers(&self) -> Result<Vec<ContactRecord>> {
        let pending = self
            .storage
            .transaction(|tx| {
                let mut records = tx
                    .contact_records()
                    .into_iter()
                    .filter(|record| {
                        matches!(
                            record.public_contact_marker_status,
                            PublicationStatus::PendingPublication
                                | PublicationStatus::PendingRemoval
                        )
                    })
                    .collect::<Vec<_>>();
                records.sort_by(|left, right| {
                    let left_status = match left.public_contact_marker_status {
                        PublicationStatus::PendingRemoval => 0,
                        PublicationStatus::PendingPublication => 1,
                        _ => 2,
                    };
                    let right_status = match right.public_contact_marker_status {
                        PublicationStatus::PendingRemoval => 0,
                        PublicationStatus::PendingPublication => 1,
                        _ => 2,
                    };
                    left_status
                        .cmp(&right_status)
                        .then_with(|| left.public_key.as_str().cmp(right.public_key.as_str()))
                });
                Ok(records)
            })
            .await?;
        let mut synced = Vec::new();
        for record in pending {
            if record.public_contact_marker_status == PublicationStatus::PendingPublication
                && self.config.public_contact_sharing != PublicContactSharingPolicy::Enabled
            {
                continue;
            }
            let session_access = self
                .load_session_access_for_initialized_identity("sync public contact markers")
                .await?;
            let public_key = record.public_key;
            let result = paykit_lib::with_write_lock(
                &session_access.session,
                &public_contact_path(&public_key),
                |lock| async {
                    // The pending list can be stale by the time this marker is locked.
                    let current = self
                        .storage
                        .transaction(|tx| Ok(tx.contact_record(&public_key)))
                        .await?;
                    match current.map(|record| record.public_contact_marker_status) {
                        Some(PublicationStatus::PendingPublication)
                            if self.config.public_contact_sharing
                                == PublicContactSharingPolicy::Enabled =>
                        {
                            self.publish_public_contact_locked(
                                &session_access.session,
                                lock,
                                &public_key,
                            )
                            .await
                            .map(Some)
                        }
                        Some(PublicationStatus::PendingRemoval) => {
                            self.remove_public_contact_locked(
                                &session_access.session,
                                lock,
                                &public_key,
                            )
                            .await
                        }
                        _ => Ok(None),
                    }
                },
            )
            .await?;
            if let Some(record) = result {
                synced.push(record);
            }
        }
        Ok(synced)
    }

    async fn mark_public_contact_failed(
        &self,
        public_key: &PubkyPublicKey,
        pending_status: PublicationStatus,
        error: String,
    ) -> Result<()> {
        let failed_at = self.clock.now();
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(());
                };
                require_pending_contact_marker(&existing, pending_status)?;
                tx.save_contact_record(existing.mark_public_contact_failed(error, failed_at));
                Ok(())
            })
            .await
    }
}

fn save_contact_in_transaction(
    tx: &mut dyn StorageTransaction,
    update: ContactUpdate,
    now: DateTime<Utc>,
) -> ContactRecord {
    let existing = tx.contact_record(&update.public_key);
    let record = ContactRecord::from_update(update, existing, now);
    tx.save_contact_record(record.clone());
    record
}

fn require_pending_contact_marker(
    record: &ContactRecord,
    expected: PublicationStatus,
) -> Result<()> {
    if record.public_contact_marker_status != expected {
        return Err(PaykitSdkError::ConcurrentUpdate {
            context: "public contact marker intent changed during publication".into(),
            source: None,
        });
    }
    Ok(())
}
