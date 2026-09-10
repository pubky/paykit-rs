use super::*;
use sha2::{Digest, Sha256};

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
        receiver_path: PaykitReceiverPath,
    ) -> Result<Option<PaykitProfileRecord>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for profile lookup".into(),
                    source: None,
                })?;
        let path = self.config.paykit_profile_path_for_receiver(&receiver_path);
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

    /// Delete the local public Paykit profile.
    pub async fn delete_paykit_profile(&self) -> Result<()> {
        let session_access = self
            .load_session_access_for_initialized_identity("delete Paykit profile")
            .await?;
        let path = self.config.paykit_profile_path();
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
                    Err(map_pubky_transport_error("delete Paykit profile", err))
                }
            })
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

    /// Upload profile avatar bytes under the configured Paykit blob prefix.
    ///
    /// The blob name is derived from the content hash and image content type.
    /// Identical uploads share a URI. A failed proposal does not establish that
    /// this blob is unused. The caller must establish exclusive ownership before
    /// deleting it, including references from other proposals or profiles.
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
    ///
    /// This compatibility API has no byte limit. Use
    /// [`Self::fetch_pubky_file_bounded`] for untrusted files or images.
    pub async fn fetch_pubky_file(&self, uri: &str) -> Result<Option<Vec<u8>>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky file fetch".into(),
                    source: None,
                })?;
        fetch_public_file_uri(&public_storage, uri, "fetch Pubky file", None).await
    }

    /// Fetch a public file while limiting the accumulated successful response body.
    ///
    /// Missing files return `None`. Successful bodies larger than `max_bytes` return a
    /// protocol error, including streams without Content-Length. The limit is
    /// checked before each chunk is appended and the response is dropped on
    /// overflow. Transport buffers and the current chunk are additional memory.
    ///
    /// HTTP error bodies are not bounded: the current Pubky client buffers them
    /// before Paykit regains control. Closing that gap through this API requires
    /// a Pubky client API change. This is not complete response-size protection.
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
        fetch_public_file_uri(&public_storage, uri, "fetch Pubky file", Some(max_bytes)).await
    }

    /// Fetch a public `pubky://` text file referenced by profile metadata.
    pub async fn fetch_pubky_text(&self, uri: &str) -> Result<Option<String>> {
        let Some(bytes) = self.fetch_pubky_file(uri).await? else {
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
        receiver_path: PaykitReceiverPath,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<ContactProfileResolution>> {
        if let Some(record) = self
            .fetch_paykit_profile(public_key.clone(), receiver_path)
            .await?
        {
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

    /// Resolve a public profile, preferring Paykit Profile.
    pub async fn resolve_profile(
        &self,
        public_key: PubkyPublicKey,
        receiver_path: PaykitReceiverPath,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<ContactProfileResolution>> {
        self.resolve_contact_profile(public_key, receiver_path, allow_pubky_profile_fallback)
            .await
    }

    /// Resolve this identity's public profile.
    pub async fn current_profile(
        &self,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<ContactProfileResolution>> {
        let public_key = self
            .require_initialized_identity("resolve current profile")
            .await?;
        self.resolve_profile(
            public_key,
            self.config.receiver_path.clone(),
            allow_pubky_profile_fallback,
        )
        .await
    }
}

pub(super) async fn read_bounded_public_file(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let too_large = || PaykitSdkError::Protocol {
        context: format!("public file exceeds the {max_bytes} byte limit"),
        source: None,
    };
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| PaykitSdkError::Transport {
            context: "read public file".into(),
            source: Some(err.into()),
        })?
    {
        if chunk.len() as u64 > max_bytes.saturating_sub(bytes.len() as u64) {
            return Err(too_large());
        }
        bytes
            .try_reserve_exact(chunk.len())
            .map_err(|_| PaykitSdkError::Protocol {
                context: "public file cannot fit in memory".into(),
                source: None,
            })?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    async fn response(
        raw: &'static str,
        hold_open: bool,
    ) -> (reqwest::Response, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream.write_all(raw.as_bytes()).unwrap();
            if hold_open {
                // No EOF or terminating chunk until the bounded reader drops
                // the connection. Full-body buffering would wait indefinitely.
                let mut byte = [0];
                let _ = stream.read(&mut byte);
            }
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/image"))
            .send()
            .await
            .unwrap();
        (response, server)
    }

    #[tokio::test]
    async fn test_bounded_public_file_stops_oversized_stream_before_eof() {
        let (response, server) = response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n",
            true,
        )
        .await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_bounded_public_file(response, 6),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn test_bounded_public_file_rejects_declared_size_before_body() {
        let (response, server) =
            response("HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n", true).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_bounded_public_file(response, 6),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn test_bounded_public_file_accepts_boundary_and_empty_bodies() {
        for (raw, limit, expected) in [
            ("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n12\r\n2\r\n34\r\n0\r\n\r\n", 4, "1234"),
            ("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n1234", 4, "1234"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", 0, ""),
        ] {
            let (response, server) = response(raw, false).await;
            assert_eq!(read_bounded_public_file(response, limit).await.unwrap(), expected.as_bytes());
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn test_bounded_public_file_reports_truncated_body() {
        let (response, server) =
            response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n12", false).await;
        assert!(matches!(
            read_bounded_public_file(response, 4).await,
            Err(PaykitSdkError::Transport { .. })
        ));
        server.join().unwrap();
    }
}
