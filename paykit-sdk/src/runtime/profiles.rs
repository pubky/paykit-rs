use super::*;
use sha2::{Digest, Sha256};

const MAX_PUBLIC_PROFILE_BYTES: usize = 256 * 1024;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Publish the local public Paykit profile.
    ///
    /// This writes the identity-wide Paykit Profile path. Use Paykit Blob
    /// helpers for profile files stored under the Paykit blob prefix. Pass
    /// `None` to create the first profile, or the revision returned by a prior
    /// fetch/publication to replace that exact version. A stale revision
    /// returns [`PaykitSdkError::ConcurrentUpdate`].
    pub async fn publish_paykit_profile(
        &self,
        profile: PaykitProfile,
        expected_revision: Option<String>,
    ) -> Result<PaykitProfileRecord> {
        let json = profile_json(&profile)?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for profile publication".into(),
            source: None,
        })?;
        let path = PAYKIT_PROFILE_PATH;
        let revision = paykit_lib::content_revision(json.as_bytes());
        paykit_lib::put_resource_if_revision(
            &session_access.session,
            path,
            json.into_bytes(),
            expected_revision.as_deref(),
            MAX_PUBLIC_PROFILE_BYTES,
        )
        .await?;
        Ok(PaykitProfileRecord {
            public_key: session_access.public_key()?,
            profile,
            path: path.to_owned(),
            revision,
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
        let path = PAYKIT_PROFILE_PATH;
        let Some((raw_json, revision)) = fetch_public_text_with_revision(
            &public_storage,
            &public_key,
            path,
            "fetch profile",
            MAX_PUBLIC_PROFILE_BYTES,
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(PaykitProfileRecord {
            public_key,
            profile: parse_profile_json(&raw_json)?,
            path: path.to_owned(),
            revision,
            updated_at: self.clock.now(),
        }))
    }

    /// Delete the local public Paykit profile at the supplied revision.
    pub async fn delete_paykit_profile(&self, expected_revision: String) -> Result<()> {
        let session_access = self
            .load_session_access_for_initialized_identity("delete Paykit profile")
            .await?;
        let path = PAYKIT_PROFILE_PATH;
        paykit_lib::delete_resource_if_revision(
            &session_access.session,
            path,
            &expected_revision,
            MAX_PUBLIC_PROFILE_BYTES,
        )
        .await
        .map_err(Into::into)
    }

    /// Publish a public blob under the identity-wide Paykit blob prefix.
    pub async fn publish_paykit_blob(
        &self,
        blob_name: String,
        bytes: Vec<u8>,
    ) -> Result<PaykitBlobRecord> {
        let path = paykit_blob_path(PAYKIT_PROFILE_BLOB_PATH_PREFIX, &blob_name)?;
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

    /// Upload profile avatar bytes under the identity-wide Paykit blob prefix.
    ///
    /// The blob name is derived from the content hash and image content type.
    pub async fn upload_profile_avatar(
        &self,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<PaykitBlobRecord> {
        if bytes.is_empty() {
            return Err(PaykitSdkError::Protocol {
                context: "profile avatar bytes must not be empty".into(),
                source: None,
            });
        }
        let extension = avatar_extension(content_type)?;
        let digest = Sha256::digest(&bytes);
        let blob_name = format!("avatar-{}.{extension}", hex::encode(&digest[..8]));
        self.publish_paykit_blob(blob_name, bytes).await
    }

    /// Delete a public blob from the identity-wide Paykit blob prefix.
    pub async fn delete_paykit_blob(&self, uri_or_path: &str) -> Result<()> {
        let session_access = self
            .load_session_access_for_initialized_identity("delete Paykit blob")
            .await?;
        let public_key = session_access.public_key()?;
        let path = paykit_blob_path_from_uri_or_path(
            &public_key,
            PAYKIT_PROFILE_BLOB_PATH_PREFIX,
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
    ///
    /// The caller supplies a positive byte limit.
    pub async fn fetch_pubky_file(&self, uri: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        require_positive_response_limit(max_bytes, "fetch Pubky file")?;
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky file fetch".into(),
                    source: None,
                })?;
        fetch_public_file_uri(&public_storage, uri, "fetch Pubky file", max_bytes).await
    }

    /// Fetch a public file while limiting the accumulated successful response body.
    ///
    /// Missing files return `None`. The effective limit is the smaller of `max_bytes`
    /// and 5 MiB. Larger successful bodies return a protocol error, including
    /// streams without Content-Length. The limit is
    /// checked before each chunk is appended and the response is dropped on
    /// overflow. Transport buffers and the current chunk are additional memory.
    ///
    /// HTTP error bodies are not bounded: the current Pubky client buffers them
    /// before Paykit regains control. Closing that gap through this API requires
    /// a Pubky client API change. This is not complete response-size protection.
    /// A hostile homeserver can bypass the limit by returning an HTTP error status.
    /// A Pubky request timeout limits that request's duration, not its memory use.
    ///
    /// Zero permits only an empty successful body. This does not decode images or limit
    /// pixels, cache storage, or request duration. Pubky client configuration,
    /// sessions, capabilities, and key rotation remain the caller's responsibility.
    pub async fn fetch_pubky_file_bounded(
        &self,
        uri: &str,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky file fetch".into(),
                    source: None,
                })?;
        fetch_public_file_uri(
            &public_storage,
            uri,
            "fetch Pubky file",
            max_bytes.min(5 * 1024 * 1024) as usize,
        )
        .await
    }

    /// Fetch a public `pubky://` text file referenced by profile metadata.
    pub async fn fetch_pubky_text(&self, uri: &str, max_bytes: usize) -> Result<Option<String>> {
        require_positive_response_limit(max_bytes, "fetch Pubky text")?;
        let Some(bytes) = self.fetch_pubky_file(uri, max_bytes).await? else {
            return Ok(None);
        };
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|err| PaykitSdkError::Protocol {
                context: format!("fetch Pubky text: invalid UTF-8: {err}"),
                source: None,
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
            MAX_PUBLIC_PROFILE_BYTES,
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

    /// List one page of immediate children in a public Pubky directory.
    ///
    /// `uri` must address a `/pub/` directory ending in `/`. `limit` must be
    /// between 1 and 100. Pass the last returned URI as `cursor` for the next
    /// page, and continue until an empty page; homeservers may return fewer
    /// entries than requested. Missing directories also return an empty page.
    /// Listings are not snapshots: restart from `None` to discover entries added
    /// before an earlier cursor. Entries are returned as canonical `pubky://` URIs.
    ///
    /// This read uses public storage without authentication. Pubky client
    /// configuration, sessions, capability scope and key rotation remain the
    /// caller's responsibility. The entry limit is not a response byte limit:
    /// the Pubky client buffers the listing before Paykit validates its entries.
    pub async fn list_pubky_directory_page(
        &self,
        uri: &str,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<Vec<String>> {
        let (directory, cursor) = public_directory_request(uri, cursor, limit)?;
        let storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for directory listing".into(),
                    source: None,
                })?;
        let mut request = storage
            .list(directory.clone())
            .map_err(|err| map_pubky_transport_error("list Pubky directory", err))?
            .shallow(true)
            .limit(limit);
        if let Some(cursor) = cursor.as_ref() {
            // The homeserver expects a decoded storage cursor inside the query value.
            request = request.cursor(decode_directory_uri(&cursor.to_pubky_url())?.as_ref());
        }
        let entries = match request.send().await {
            Ok(entries) => entries,
            Err(err) if is_pubky_not_found(&err) => return Ok(Vec::new()),
            Err(err) => return Err(map_pubky_transport_error("list Pubky directory", err)),
        };
        public_directory_page(&directory, cursor.as_ref(), limit, entries)
    }

    /// Fetch public Pubky app follows.
    pub async fn fetch_pubky_follows(
        &self,
        public_key: PubkyPublicKey,
        max_entries: usize,
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
            max_entries,
        )
        .await?;
        Ok(pubky_follow_keys_from_follow_entries(entries))
    }

    /// Resolve a public profile, preferring Paykit Profile.
    pub async fn resolve_profile(
        &self,
        public_key: PubkyPublicKey,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<ProfileResolution>> {
        if let Some(record) = self.fetch_paykit_profile(public_key.clone()).await? {
            return Ok(Some(ProfileResolution::from_paykit(record)));
        }
        if allow_pubky_profile_fallback {
            return self
                .fetch_pubky_profile(public_key)
                .await
                .map(|record| record.map(ProfileResolution::from_pubky));
        }
        Ok(None)
    }

    /// Resolve this identity's public profile.
    pub async fn current_profile(
        &self,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<ProfileResolution>> {
        let public_key = self
            .require_initialized_identity("resolve current profile")
            .await?;
        self.resolve_profile(public_key, allow_pubky_profile_fallback)
            .await
    }
}

fn avatar_extension(content_type: &str) -> Result<&'static str> {
    match content_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Ok("jpg"),
        "image/png" => Ok("png"),
        "image/webp" => Ok("webp"),
        "image/gif" => Ok("gif"),
        _ => Err(PaykitSdkError::Protocol {
            context: format!("unsupported profile avatar content type: {content_type}"),
            source: None,
        }),
    }
}

fn directory_error(message: &str) -> PaykitSdkError {
    PaykitSdkError::Protocol {
        context: format!("list Pubky directory: {message}"),
        source: None,
    }
}

fn decode_directory_uri(uri: &str) -> Result<std::borrow::Cow<'_, str>> {
    percent_encoding::percent_decode_str(uri)
        .decode_utf8()
        .map_err(|_| directory_error("URI must contain valid UTF-8"))
}

fn public_directory_child(directory: &pubky::PubkyResource, entry: &pubky::PubkyResource) -> bool {
    directory.owner == entry.owner
        && entry
            .path
            .as_str()
            .strip_prefix(directory.path.as_str())
            .is_some_and(|suffix| !suffix.is_empty() && !suffix.trim_end_matches('/').contains('/'))
}

fn public_directory_request(
    uri: &str,
    cursor: Option<&str>,
    limit: u16,
) -> Result<(pubky::PubkyResource, Option<pubky::PubkyResource>)> {
    let directory: pubky::PubkyResource = decode_directory_uri(uri)?
        .parse()
        .map_err(|_| directory_error("invalid URI"))?;
    if !directory.path.as_str().starts_with("/pub/") || !directory.path.as_str().ends_with('/') {
        return Err(directory_error(
            "URI must name a public directory ending in /",
        ));
    }
    if !(1..=100).contains(&limit) {
        return Err(directory_error("entry limit must be between 1 and 100"));
    }
    let cursor = cursor
        .map(|uri| {
            let entry: pubky::PubkyResource = decode_directory_uri(uri)?
                .parse()
                .map_err(|_| directory_error("invalid cursor"))?;
            if !public_directory_child(&directory, &entry) {
                return Err(directory_error(
                    "cursor must be a child of the requested directory",
                ));
            }
            Ok(entry)
        })
        .transpose()?;
    Ok((directory, cursor))
}

fn public_directory_page(
    directory: &pubky::PubkyResource,
    cursor: Option<&pubky::PubkyResource>,
    limit: u16,
    entries: Vec<pubky::PubkyResource>,
) -> Result<Vec<String>> {
    if entries.len() > usize::from(limit) {
        return Err(directory_error(
            "response exceeds the requested entry limit",
        ));
    }
    let mut result = Vec::with_capacity(entries.len());
    // Homeserver COLLATE C order is decoded storage-path byte order, not URI order.
    let storage_key = |entry: &pubky::PubkyResource| {
        percent_encoding::percent_decode_str(entry.path.as_str()).collect::<Vec<_>>()
    };
    let mut previous = cursor.map(storage_key);
    for entry in entries {
        if !public_directory_child(directory, &entry) {
            return Err(directory_error(
                "response contains an entry outside the requested directory",
            ));
        }
        let key = storage_key(&entry);
        if previous.as_ref().is_some_and(|previous| key <= *previous) {
            return Err(directory_error("directory cursor did not advance"));
        }
        previous = Some(key);
        result.push(entry.to_pubky_url());
    }
    Ok(result)
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    #[test]
    fn test_public_directory_orders_decoded_storage_paths() {
        let owner = pubky::Keypair::random().public_key();
        let directory = pubky::PubkyResource::new(owner.clone(), "/pub/example/").unwrap();
        let entries: Vec<_> = ["a", "\u{e9}", "\u{ea}"]
            .into_iter()
            .map(|name| {
                pubky::PubkyResource::new(owner.clone(), format!("/pub/example/{name}")).unwrap()
            })
            .collect();
        let expected: Vec<_> = entries.iter().map(|entry| entry.to_pubky_url()).collect();
        assert_eq!(
            public_directory_page(&directory, None, 3, entries.clone()).unwrap(),
            expected
        );
        for index in 0..entries.len() - 1 {
            assert_eq!(
                public_directory_page(
                    &directory,
                    Some(&entries[index]),
                    1,
                    vec![entries[index + 1].clone()]
                )
                .unwrap(),
                vec![expected[index + 1].clone()]
            );
        }
        assert!(
            public_directory_page(&directory, Some(&entries[1]), 1, vec![entries[1].clone()])
                .is_err()
        );
    }

    #[test]
    fn test_public_directory_roundtrips_encoded_paths_and_cursors() {
        let owner = pubky::Keypair::random().public_key();
        for path in ["/pub/example/", "/pub/\u{e9}/", "/pub/100% ready/"] {
            let directory = pubky::PubkyResource::new(owner.clone(), path).unwrap();
            for name in ["\u{e9}", "100% ready", "%C3%A9"] {
                let entry =
                    pubky::PubkyResource::new(owner.clone(), format!("{path}{name}")).unwrap();
                let uri = entry.to_pubky_url();
                let (parsed, cursor) =
                    public_directory_request(&directory.to_pubky_url(), Some(&uri), 1).unwrap();
                assert_eq!(parsed, directory);
                assert_eq!(cursor, Some(entry));
            }
        }
    }

    #[test]
    fn test_public_directory_validates_scope_limits_and_progress() {
        let owner = pubky::Keypair::random().public_key().z32();
        let root = format!("pubky://{owner}/pub/example/");
        let first = format!("{root}a.json");
        let second = format!("{root}b.json");
        let (directory, cursor) = public_directory_request(&root, Some(&first), 2).unwrap();
        assert_eq!(
            cursor.as_ref().map(|entry| entry.to_pubky_url()),
            Some(first.clone())
        );
        assert_eq!(
            public_directory_page(
                &directory,
                cursor.as_ref(),
                2,
                vec![second.parse().unwrap()]
            )
            .unwrap(),
            vec![second.clone()]
        );
        for invalid in [
            format!("pubky://{owner}/priv/example/"),
            format!("{root}file"),
            format!("{root}/"),
            format!("{root}%FF/"),
            format!("{root}%2e%2e/"),
        ] {
            assert!(public_directory_request(&invalid, None, 2).is_err());
        }
        for limit in [0, 101, u16::MAX] {
            assert!(public_directory_request(&root, None, limit).is_err());
        }
        for invalid in [
            root.clone(),
            format!("{root}nested/a"),
            format!("pubky://{owner}/pub/other/a"),
            format!(
                "pubky://{}/pub/example/a",
                pubky::Keypair::random().public_key().z32()
            ),
        ] {
            assert!(public_directory_request(&root, Some(&invalid), 2).is_err());
            assert!(
                public_directory_page(&directory, None, 2, vec![invalid.parse().unwrap()]).is_err()
            );
        }
        assert!(public_directory_request(&root, Some(&format!("{root}nested%2Fa")), 2).is_err());
        for entries in [
            vec![first.clone()],
            vec![second.clone(), first.clone()],
            vec![second.clone(), second.clone()],
        ] {
            assert!(public_directory_page(
                &directory,
                Some(&first.parse().unwrap()),
                2,
                entries.iter().map(|uri| uri.parse().unwrap()).collect()
            )
            .is_err());
        }
        assert!(public_directory_page(
            &directory,
            None,
            1,
            vec![first.parse().unwrap(), second.parse().unwrap()]
        )
        .is_err());
    }
}
