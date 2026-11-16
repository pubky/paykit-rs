use super::*;
use crate::domain::contacts::{
    merge_pubky_profile_json, pubky_app_avatar_file_json, pubky_app_blob_id,
    pubky_app_timestamp_id, social_v1_file_name, PubkyProfileSnapshot, PubkyProfileUpdate,
    SocialVersion, PUBKY_APP_BLOB_PATH_PREFIX, PUBKY_APP_FILE_PATH_PREFIX,
    PUBKY_APP_PROFILE_SESSION_CAPABILITIES, SOCIAL_V1_FILE_PATH_PREFIX,
    SOCIAL_V1_MIGRATION_FLAG_CAPABILITY, SOCIAL_V1_MIGRATION_FLAG_PATH, SOCIAL_V1_PROFILE_PATH,
};

const MAX_PUBKY_PROFILE_BYTES: usize = 256 * 1024;
// The flag lists every migrated object path; the pubky.app migrator reads it up to 64 MiB.
const MAX_MIGRATION_FLAG_BYTES: usize = 64 * 1024 * 1024;
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
    /// elsewhere since this read is never overwritten. The signed-in identity's
    /// profile is read from the layout pubky.app's migration flag names when the
    /// session may read the flag; any other profile from Pubky social v1 when it
    /// has one there, else from pubky.app (v0).
    pub async fn fetch_pubky_profile_with_revision(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<PubkyProfileSnapshot>> {
        let version = self.readable_own_social_version(&public_key).await?;
        let storage = self.pubky_profile_storage().await?;
        let fetch = |path| {
            fetch_public_text_with_revision(
                &storage,
                &public_key,
                path,
                "fetch Pubky profile",
                MAX_PUBKY_PROFILE_BYTES,
            )
        };
        let found = match version {
            Some(version) => fetch(version.profile_path()).await?,
            None => match fetch(SOCIAL_V1_PROFILE_PATH).await? {
                Some(found) => Some(found),
                None => fetch(PUBKY_PROFILE_PATH).await?,
            },
        };
        let Some((raw_json, revision)) = found else {
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
    /// the stored profile no longer matches `expected_revision`. It goes to the
    /// layout pubky.app's migration flag names: Pubky social v1 once pubky.app
    /// migrated the identity, else pubky.app (v0). The session must hold
    /// [`PUBKY_APP_PROFILE_SESSION_CAPABILITIES`], else this returns
    /// [`PaykitSdkError::Identity`]; session capabilities remain the caller's
    /// responsibility.
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
        let version = own_social_version(&session_access).await?;
        let path = version.profile_path();
        let current = match self.fetch_pubky_profile_raw(&public_key, path).await? {
            Some((raw, revision)) if Some(&revision) == expected_revision.as_ref() => Some(raw),
            None if expected_revision.is_none() => None,
            _ => return Err(pubky_profile_changed()),
        };
        let bytes = merge_pubky_profile_json(current.as_deref(), &update, version)?;
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
            path,
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

    /// Delete this identity's Pubky app profile at the revision it was read at.
    ///
    /// An absent profile is already deleted. A profile that changed since
    /// `expected_revision` returns [`PaykitSdkError::ConcurrentUpdate`]. A
    /// migrated identity's profile is deleted from Pubky social v1 and its
    /// pubky.app (v0) copy with it, as pubky.app deletes it. The session must
    /// hold [`PUBKY_APP_PROFILE_SESSION_CAPABILITIES`].
    pub async fn delete_pubky_profile(&self, expected_revision: String) -> Result<()> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for Pubky profile deletion".into(),
            source: None,
        })?;
        let version = own_social_version(&session_access).await?;
        paykit_lib::delete_resource_if_revision(
            &session_access.session,
            version.profile_path(),
            &expected_revision,
            MAX_PUBKY_PROFILE_BYTES,
        )
        .await
        .map_err(scope_error)?;
        if version == SocialVersion::V1 {
            match session_access
                .session
                .storage()
                .delete(PUBKY_PROFILE_PATH)
                .await
            {
                Ok(_) => {}
                Err(err) if is_pubky_not_found(&err) => {}
                Err(err) => return Err(put_error("delete Pubky app (v0) profile", err)),
            }
        }
        Ok(())
    }

    /// Upload avatar bytes and return the file URI to set as the profile `image`.
    ///
    /// A migrated identity's avatar is one Pubky social v1 file, the raw bytes
    /// named by their content hash and extension. Otherwise it is a pubky.app
    /// (v0) blob and file: the blob id is the content hash and the file id the
    /// upload time, as `pubky-app-specs` defines them. The session must hold
    /// [`PUBKY_APP_PROFILE_SESSION_CAPABILITIES`].
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
        if own_social_version(&session_access).await? == SocialVersion::V1 {
            let path = format!(
                "{SOCIAL_V1_FILE_PATH_PREFIX}{}",
                social_v1_file_name(&bytes, content_type)
            );
            storage
                .put(path.as_str(), bytes)
                .await
                .map_err(|err| put_error("upload Pubky social v1 avatar", err))?;
            return Ok(public_resource_uri(&public_key, &path));
        }

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

    /// The layout of `public_key`'s profile per pubky.app's migration flag, when
    /// `public_key` is the signed-in identity and its session may read the flag.
    pub(super) async fn readable_own_social_version(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<SocialVersion>> {
        let _session_guard = self.session_read_guard().await?;
        let Some(session_access) = self.pubky.load_session_access().await? else {
            return Ok(None);
        };
        if session_access.public_key()? != *public_key
            || session_access
                .validate_for_capabilities(SOCIAL_V1_MIGRATION_FLAG_CAPABILITY)
                .is_err()
        {
            return Ok(None);
        }
        read_social_version(&session_access).await.map(Some)
    }

    async fn pubky_profile_storage(&self) -> Result<pubky::PublicStorage> {
        self.pubky
            .load_public_storage()
            .await?
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "no Pubky public storage available for Pubky profile lookup".into(),
                source: None,
            })
    }

    async fn fetch_pubky_profile_raw(
        &self,
        public_key: &PubkyPublicKey,
        path: &str,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let storage = self.pubky_profile_storage().await?;
        Ok(fetch_public_text_with_revision(
            &storage,
            public_key,
            path,
            "fetch Pubky profile",
            MAX_PUBKY_PROFILE_BYTES,
        )
        .await?
        .map(|(raw, revision)| (raw.into_bytes(), revision)))
    }
}

/// The layout this identity writes its profile, avatar and follows in, as
/// pubky.app reads it from its migration flag. The session must hold
/// [`PUBKY_APP_PROFILE_SESSION_CAPABILITIES`], so a session that cannot read the
/// flag is signed in again rather than writing a layout pubky.app no longer reads.
async fn own_social_version(session_access: &PubkySessionAccess) -> Result<SocialVersion> {
    session_access
        .validate_for_capabilities(PUBKY_APP_PROFILE_SESSION_CAPABILITIES)
        .map_err(|_| missing_scope())?;
    read_social_version(session_access).await
}

async fn read_social_version(session_access: &PubkySessionAccess) -> Result<SocialVersion> {
    const CONTEXT: &str = "read Pubky social v1 migration flag";
    let flag = match session_access
        .session
        .storage()
        .get(SOCIAL_V1_MIGRATION_FLAG_PATH)
        .await
    {
        Ok(response) => read_public_response(response, MAX_MIGRATION_FLAG_BYTES, CONTEXT).await?,
        Err(err) if is_pubky_not_found(&err) => None,
        Err(err) => return Err(put_error(CONTEXT, err)),
    };
    Ok(SocialVersion::from_migration_flag(flag.as_deref()))
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
