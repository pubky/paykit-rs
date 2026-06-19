use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Save or update a local contact record.
    pub async fn save_contact(&self, update: ContactUpdate) -> Result<ContactRecord> {
        update.validate()?;
        let local_public_key = self.require_initialized_identity("save contact").await?;
        if update.public_key == local_public_key {
            return Err(PaykitSdkError::Policy(
                "cannot save the local Paykit identity as a contact".into(),
            ));
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let existing = tx.contact_record(&update.public_key);
                let record = ContactRecord::from_update(update, existing, now);
                tx.save_contact_record(record.clone());
                Ok(record)
            })
            .await
    }

    /// Load one local contact record.
    pub async fn contact_record(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.require_initialized_identity("load contact").await?;
        self.storage
            .transaction(|tx| Ok(tx.contact_record(public_key)))
            .await
    }

    /// List local contact records.
    pub async fn contact_records(&self) -> Result<Vec<ContactRecord>> {
        self.require_initialized_identity("list contacts").await?;
        self.storage
            .transaction(|tx| Ok(tx.contact_records()))
            .await
    }

    /// Remove one local contact record.
    pub async fn remove_contact(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.require_initialized_identity("remove contact").await?;
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(None);
                };
                if !existing.can_remove_locally() {
                    return Err(PaykitSdkError::Policy(format!(
                        "remove public contact marker before deleting contact {public_key}"
                    )));
                }
                Ok(tx.remove_contact_record(public_key))
            })
            .await
    }

    /// Fetch a contact's public profile and cache it in the local contact record.
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

    /// Publish a public contact marker for a saved local contact.
    ///
    /// This can reveal part of the local contact graph. It only runs when
    /// `public_contact_sharing` is `ConfiguredPublicNamespace`.
    pub async fn publish_public_contact(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<ContactRecord> {
        if self.config.public_contact_sharing
            != PublicContactSharingPolicy::ConfiguredPublicNamespace
        {
            return Err(PaykitSdkError::Policy(
                "public contact sharing is disabled".into(),
            ));
        }
        let session_access = self
            .load_session_access_for_initialized_identity("publish public contact")
            .await?;
        let pending_at = self.clock.now();
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Err(PaykitSdkError::Protocol(format!(
                        "cannot publish unsaved contact {public_key}"
                    )));
                };
                tx.save_contact_record(
                    existing.mark_public_contact_publication_pending(pending_at),
                );
                Ok(())
            })
            .await?;
        let path = self.config.public_contact_path(&public_key);
        let write_result = session_access
            .session
            .storage()
            .put(path.as_str(), public_contact_json(&public_key)?)
            .await
            .map_err(|err| map_pubky_transport_error("publish public contact", err));
        if let Err(err) = write_result {
            self.mark_public_contact_failed(&public_key, err.to_string())
                .await?;
            return Err(err);
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Err(PaykitSdkError::Protocol(format!(
                        "contact {public_key} disappeared before public publication was recorded"
                    )));
                };
                let record = existing.mark_public_contact_published(now);
                tx.save_contact_record(record.clone());
                Ok(record)
            })
            .await
    }

    /// Remove a public contact marker for a saved local contact.
    ///
    /// Cleanup is allowed even when public contact sharing is disabled, so an
    /// app can stop publishing markers and still remove previously published
    /// markers.
    pub async fn remove_public_contact(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        let existing_record = self
            .storage
            .transaction(|tx| Ok(tx.contact_record(&public_key)))
            .await?;
        let session_access = self
            .load_session_access_for_initialized_identity("remove public contact")
            .await?;
        let had_local_record = existing_record.is_some();
        if existing_record
            .as_ref()
            .is_some_and(ContactRecord::may_have_public_marker)
        {
            let pending_at = self.clock.now();
            self.storage
                .transaction(|tx| {
                    let Some(existing) = tx.contact_record(&public_key) else {
                        return Ok(());
                    };
                    tx.save_contact_record(
                        existing.mark_public_contact_removal_pending(pending_at),
                    );
                    Ok(())
                })
                .await?;
        }
        let path = self.config.public_contact_path(&public_key);
        let delete_result = session_access.session.storage().delete(path.as_str()).await;
        if let Err(err) = delete_result {
            if !is_pubky_not_found(&err) {
                let err = map_pubky_transport_error("remove public contact", err);
                self.mark_public_contact_failed(&public_key, err.to_string())
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
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Ok(None);
                };
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
            match record.public_contact_marker_status {
                PublicationStatus::PendingPublication => {
                    if self.config.public_contact_sharing
                        == PublicContactSharingPolicy::ConfiguredPublicNamespace
                    {
                        synced.push(self.publish_public_contact(record.public_key).await?);
                    } else {
                        self.mark_public_contact_failed(
                            &record.public_key,
                            "public contact sharing is disabled".into(),
                        )
                        .await?;
                    }
                }
                PublicationStatus::PendingRemoval => {
                    if let Some(record) = self.remove_public_contact(record.public_key).await? {
                        synced.push(record);
                    }
                }
                _ => {}
            }
        }
        Ok(synced)
    }

    async fn mark_public_contact_failed(
        &self,
        public_key: &PubkyPublicKey,
        error: String,
    ) -> Result<()> {
        let failed_at = self.clock.now();
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(());
                };
                tx.save_contact_record(existing.mark_public_contact_failed(error, failed_at));
                Ok(())
            })
            .await
    }
}
