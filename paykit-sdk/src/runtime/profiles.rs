use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
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
}
