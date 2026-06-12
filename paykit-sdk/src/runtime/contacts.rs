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
        self.require_initialized_identity("save contact").await?;
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

    /// Publish the local public Paykit profile.
    ///
    /// This writes only the configured Paykit Profile path. Use Paykit Blob
    /// helpers for profile files stored under the configured blob prefix.
    pub async fn publish_paykit_profile(
        &self,
        profile: PaykitProfile,
    ) -> Result<PaykitProfileRecord> {
        let json = profile_json(&profile)?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for profile publication".into(),
            source: None,
        })?;
        let path = self.config.paykit_profile_path();
        session_access
            .session
            .storage()
            .put(path.as_str(), json)
            .await
            .map_err(|err| map_pubky_transport_error("publish Paykit profile", err))?;
        Ok(PaykitProfileRecord {
            public_key: session_access.public_key()?,
            profile,
            path,
            updated_at: self.clock.now(),
        })
    }

    /// Fetch a public Paykit profile.
    pub async fn fetch_paykit_profile(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<PaykitProfileRecord>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for profile lookup".into(),
                    source: None,
                })?;
        let path = self.config.paykit_profile_path();
        let Some(raw_json) =
            fetch_public_text(&public_storage, &public_key, &path, "fetch profile").await?
        else {
            return Ok(None);
        };
        Ok(Some(PaykitProfileRecord {
            public_key,
            profile: parse_profile_json(&raw_json)?,
            path,
            updated_at: self.clock.now(),
        }))
    }

    /// Publish a public blob under the configured Paykit blob prefix.
    pub async fn publish_paykit_blob(
        &self,
        blob_name: String,
        bytes: Vec<u8>,
    ) -> Result<PaykitBlobRecord> {
        let path = paykit_blob_path(&self.config.paykit_profile_blob_path_prefix(), &blob_name)?;
        let size_bytes = bytes.len() as u64;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for Paykit blob publication".into(),
            source: None,
        })?;
        session_access
            .session
            .storage()
            .put(path.as_str(), bytes)
            .await
            .map_err(|err| map_pubky_transport_error("publish Paykit blob", err))?;
        let public_key = session_access.public_key()?;
        let uri = paykit_blob_uri(&public_key, &path);
        Ok(PaykitBlobRecord {
            public_key,
            path,
            uri,
            size_bytes,
            updated_at: self.clock.now(),
        })
    }

    /// Delete a public blob from the configured Paykit blob prefix.
    pub async fn delete_paykit_blob(&self, uri_or_path: &str) -> Result<()> {
        let session_access = self
            .load_session_access_for_initialized_identity("delete Paykit blob")
            .await?;
        let public_key = session_access.public_key()?;
        let path = paykit_blob_path_from_uri_or_path(
            &public_key,
            &self.config.paykit_profile_blob_path_prefix(),
            uri_or_path,
        )?;
        session_access
            .session
            .storage()
            .delete(path.as_str())
            .await
            .map(|_| ())
            .or_else(|err| {
                if is_pubky_not_found(&err) {
                    Ok(())
                } else {
                    Err(map_pubky_transport_error("delete Paykit blob", err))
                }
            })
    }

    /// Fetch a public `pubky://` file referenced by profile metadata.
    pub async fn fetch_pubky_file(&self, uri: &str) -> Result<Option<Vec<u8>>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky file fetch".into(),
                    source: None,
                })?;
        fetch_public_file_uri(&public_storage, uri, "fetch Pubky file").await
    }

    /// Fetch a public `pubky://` text file referenced by profile metadata.
    pub async fn fetch_pubky_text(&self, uri: &str) -> Result<Option<String>> {
        let Some(bytes) = self.fetch_pubky_file(uri).await? else {
            return Ok(None);
        };
        String::from_utf8(bytes).map(Some).map_err(|err| {
            PaykitSdkError::Protocol(format!("fetch Pubky text: invalid UTF-8: {err}"))
        })
    }

    /// Fetch a public Pubky app profile.
    pub async fn fetch_pubky_profile(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<PubkyProfileRecord>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky profile lookup".into(),
                    source: None,
                })?;
        let Some(raw_json) = fetch_public_text(
            &public_storage,
            &public_key,
            PUBKY_PROFILE_PATH,
            "fetch Pubky profile",
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(PubkyProfileRecord {
            public_key,
            profile: parse_pubky_profile_json(&raw_json)?,
            path: PUBKY_PROFILE_PATH.into(),
            fetched_at: self.clock.now(),
        }))
    }

    /// Fetch public Pubky app follows.
    pub async fn fetch_pubky_follows(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Vec<PubkyPublicKey>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky follows lookup".into(),
                    source: None,
                })?;
        let entries = list_public_resources(
            &public_storage,
            &public_key,
            PUBKY_FOLLOWS_PATH_PREFIX,
            "fetch Pubky follows",
        )
        .await?;
        Ok(pubky_follow_keys_from_follow_entries(entries))
    }

    /// Resolve a contact display profile, preferring Paykit Profile.
    pub async fn resolve_contact_profile(
        &self,
        public_key: PubkyPublicKey,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<ContactProfileResolution>> {
        if let Some(record) = self.fetch_paykit_profile(public_key.clone()).await? {
            return Ok(Some(ContactProfileResolution::from_paykit(record)));
        }
        if allow_pubky_profile_fallback {
            return self
                .fetch_pubky_profile(public_key)
                .await
                .map(|record| record.map(ContactProfileResolution::from_pubky));
        }
        Ok(None)
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
