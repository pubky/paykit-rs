use super::*;
use crate::domain::contacts::{
    merge_pubky_profile_json, pubky_app_avatar_file_json, pubky_app_blob_id,
    pubky_app_timestamp_id, PubkyProfileSnapshot, PubkyProfileUpdate, PUBKY_APP_BLOB_PATH_PREFIX,
    PUBKY_APP_FILE_PATH_PREFIX,
};

const MAX_PUBKY_PROFILE_BYTES: usize = 256 * 1024;
const MAX_PUBKY_APP_AVATAR_BYTES: usize = 5 * 1024 * 1024;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Fetch a Pubky app profile with the revision of the exact bytes read.
    ///
    /// Pass the revision to [`Self::publish_pubky_profile`] so an edit made
    /// elsewhere since this read is never overwritten.
    pub async fn fetch_pubky_profile_with_revision(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<PubkyProfileSnapshot>> {
        let storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky profile lookup".into(),
                    source: None,
                })?;
        let Some((raw_json, revision)) = fetch_public_text_with_revision(
            &storage,
            &public_key,
            PUBKY_PROFILE_PATH,
            "fetch Pubky profile",
            MAX_PUBKY_PROFILE_BYTES,
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(PubkyProfileSnapshot {
            public_key,
            profile: parse_pubky_profile_json(&raw_json)?,
            revision,
            fetched_at: self.clock.now(),
        }))
    }

    /// Apply `update` to this identity's Pubky app profile.
    ///
    /// Only the fields `update` names change; every other stored key is kept.
    /// Pass `None` to create the first profile, or the revision from
    /// [`Self::fetch_pubky_profile_with_revision`]. The write runs under a
    /// homeserver lock and returns [`PaykitSdkError::ConcurrentUpdate`] when
    /// the stored profile no longer matches `expected_revision`. The session
    /// must hold `/pub/pubky.app/profile.json:rw`; session capabilities remain
    /// the caller's responsibility.
    pub async fn publish_pubky_profile(
        &self,
        update: PubkyProfileUpdate,
        expected_revision: Option<String>,
    ) -> Result<PubkyProfileSnapshot> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for Pubky profile publication".into(),
            source: None,
        })?;
        let public_key = session_access.public_key()?;
        let current = match self.fetch_pubky_profile_raw(&public_key).await? {
            Some((raw, revision)) if Some(&revision) == expected_revision.as_ref() => Some(raw),
            None if expected_revision.is_none() => None,
            _ => return Err(pubky_profile_changed()),
        };
        let bytes = merge_pubky_profile_json(current.as_deref(), &update)?;
        let revision = paykit_lib::content_revision(&bytes);
        let profile = parse_pubky_profile_json(std::str::from_utf8(&bytes).map_err(|_| {
            PaykitSdkError::Protocol {
                context: "encoded Pubky profile is not UTF-8".into(),
                source: None,
            }
        })?)?;
        // The lock re-checks the revision, so the merge above used exactly the stored bytes.
        paykit_lib::put_resource_if_revision(
            &session_access.session,
            PUBKY_PROFILE_PATH,
            bytes,
            expected_revision.as_deref(),
            MAX_PUBKY_PROFILE_BYTES,
        )
        .await
        .map_err(scope_error)?;
        Ok(PubkyProfileSnapshot {
            public_key,
            profile,
            revision,
            fetched_at: self.clock.now(),
        })
    }

    /// Upload avatar bytes as a Pubky app blob and file, and return the file URI.
    ///
    /// The blob id is the content hash and the file id the upload time, as
    /// `pubky-app-specs` defines them. Set the returned URI as the profile
    /// `image`. The session must hold `/pub/pubky.app/blobs/:rw` and
    /// `/pub/pubky.app/files/:rw`.
    pub async fn upload_pubky_app_avatar(
        &self,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<String> {
        let content_type = pubky_app_image_content_type(content_type)?;
        if bytes.is_empty() || bytes.len() > MAX_PUBKY_APP_AVATAR_BYTES {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Pubky app avatar must be 1 to {MAX_PUBKY_APP_AVATAR_BYTES} bytes"
                ),
                source: None,
            });
        }
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for Pubky app avatar upload".into(),
            source: None,
        })?;
        let public_key = session_access.public_key()?;
        let storage = session_access.session.storage();

        let size = bytes.len();
        let blob_path = format!("{PUBKY_APP_BLOB_PATH_PREFIX}{}", pubky_app_blob_id(&bytes));
        storage
            .put(blob_path.as_str(), bytes)
            .await
            .map_err(|err| put_error("upload Pubky app avatar blob", err))?;

        let created_at = self.clock.now().timestamp_micros();
        let file_path = format!(
            "{PUBKY_APP_FILE_PATH_PREFIX}{}",
            pubky_app_timestamp_id(created_at)
        );
        let file = pubky_app_avatar_file_json(
            &public_resource_uri(&public_key, &blob_path),
            content_type,
            size,
            created_at,
        );
        storage
            .put(file_path.as_str(), file)
            .await
            .map_err(|err| put_error("upload Pubky app avatar file", err))?;
        Ok(public_resource_uri(&public_key, &file_path))
    }

    async fn fetch_pubky_profile_raw(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for Pubky profile lookup".into(),
                    source: None,
                })?;
        Ok(fetch_public_text_with_revision(
            &storage,
            public_key,
            PUBKY_PROFILE_PATH,
            "fetch Pubky profile",
            MAX_PUBKY_PROFILE_BYTES,
        )
        .await?
        .map(|(raw, revision)| (raw.into_bytes(), revision)))
    }
}

fn pubky_app_image_content_type(content_type: &str) -> Result<&'static str> {
    match content_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Ok("image/jpeg"),
        "image/png" => Ok("image/png"),
        "image/webp" => Ok("image/webp"),
        "image/gif" => Ok("image/gif"),
        _ => Err(PaykitSdkError::Protocol {
            context: format!("unsupported Pubky app avatar content type: {content_type}"),
            source: None,
        }),
    }
}

fn is_forbidden(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::FORBIDDEN || *status == StatusCode::UNAUTHORIZED
    )
}

/// A session without the Pubky app scopes is an identity problem the app can fix by signing in again.
fn missing_scope() -> PaykitSdkError {
    PaykitSdkError::Identity {
        context: "Pubky session lacks the Pubky app profile capabilities".into(),
        source: None,
    }
}

fn put_error(context: &'static str, err: PubkyError) -> PaykitSdkError {
    if is_forbidden(&err) {
        missing_scope()
    } else {
        map_pubky_transport_error(context, err)
    }
}

fn scope_error(err: paykit_lib::PaykitError) -> PaykitSdkError {
    match &err {
        paykit_lib::PaykitError::Transport { source, .. }
            if source
                .downcast_ref::<PubkyError>()
                .is_some_and(is_forbidden) =>
        {
            missing_scope()
        }
        _ => err.into(),
    }
}

fn pubky_profile_changed() -> PaykitSdkError {
    PaykitSdkError::ConcurrentUpdate {
        context: "Pubky profile changed since it was read; reload it and retry".into(),
        source: None,
    }
}
