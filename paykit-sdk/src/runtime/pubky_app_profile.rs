use super::*;
use crate::domain::contacts::{
    merge_pubky_profile_json, pubky_app_avatar_file_json, pubky_app_blob_id,
    pubky_app_timestamp_id, social_v1_file_name, PubkyProfileSnapshot, PubkyProfileUpdate,
    SocialVersion, AVATAR_FILE_NAME, PUBKY_APP_BLOB_PATH_PREFIX, PUBKY_APP_FILE_PATH_PREFIX,
    PUBKY_APP_PROFILE_SESSION_CAPABILITIES, SOCIAL_V1_FILE_PATH_PREFIX,
    SOCIAL_V1_MIGRATION_FLAG_CAPABILITY, SOCIAL_V1_MIGRATION_FLAG_PATH, SOCIAL_V1_PROFILE_PATH,
};
use crate::parse_pubky_resource;

const MAX_PUBKY_PROFILE_BYTES: usize = 256 * 1024;
// The flag lists every migrated object path; the pubky.app migrator reads it up to 64 MiB.
const MAX_MIGRATION_FLAG_BYTES: usize = 64 * 1024 * 1024;
const MAX_PUBKY_APP_AVATAR_BYTES: usize = 5 * 1024 * 1024;
const PUBKY_APP_PATH_PREFIX: &str = "/pub/pubky.app/";

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
    /// profile is read from Pubky social v1 once pubky.app's migration flag
    /// says so or it has a social v1 profile, when the session may read the
    /// flag; any other profile from Pubky social v1 when it has one there, else
    /// from pubky.app (v0).
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
    /// the stored profile no longer matches `expected_revision`. It goes to
    /// Pubky social v1 once pubky.app migrated the identity or it has a social
    /// v1 profile, else to pubky.app (v0). A first profile for an identity with
    /// nothing under `/pub/pubky.app/` is created on Pubky social v1, without
    /// pubky.app's migration flag, which only pubky.app writes. The session must hold
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
        let version = match own_social_version(&session_access).await? {
            SocialVersion::V0
                if expected_revision.is_none() && !has_pubky_app_data(&session_access).await? =>
            {
                SocialVersion::V1
            }
            version => version,
        };
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
    /// The whole delete holds the homeserver locks of both profile paths,
    /// pubky.app (v0) first, so a profile write that takes the lock on either
    /// layout waits for it; an unlocked write already in flight can still land. An absent profile is already deleted. A profile whose bytes changed
    /// since `expected_revision` returns [`PaykitSdkError::ConcurrentUpdate`]
    /// before anything is deleted. A social v1 identity's pubky.app (v0) copy is
    /// deleted first and its social v1 profile after it, in the order of the
    /// spec's `deletionPaths`. Then the avatars the deleted profiles named in
    /// this identity's own storage are deleted, best effort: a social v1 file,
    /// or a pubky.app `avatar` file and its blob. An avatar that cannot be read
    /// or deleted is logged and left, since the profile is already gone. The
    /// session must hold [`PUBKY_APP_PROFILE_SESSION_CAPABILITIES`].
    pub async fn delete_pubky_profile(&self, expected_revision: String) -> Result<()> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for Pubky profile deletion".into(),
            source: None,
        })?;
        let public_key = session_access.public_key()?;
        let version = own_social_version(&session_access).await?;
        let session = &session_access.session;
        let storage = session.storage();
        let (public_key, storage, expected_revision) = (&public_key, &storage, &expected_revision);
        paykit_lib::with_write_lock(session, PUBKY_PROFILE_PATH, |v0_lock| async move {
            let v0_lock = &v0_lock;
            paykit_lib::with_write_lock(session, SOCIAL_V1_PROFILE_PATH, |v1_lock| async move {
                let (profile_lock, copy_lock) = match version {
                    SocialVersion::V1 => (&v1_lock, Some(v0_lock)),
                    SocialVersion::V0 => (v0_lock, None),
                };
                let mut images = Vec::new();
                let current = match storage.get(version.profile_path()).await {
                    Ok(response) => {
                        read_public_response(
                            response,
                            MAX_PUBKY_PROFILE_BYTES,
                            "read Pubky profile",
                        )
                        .await?
                    }
                    Err(err) if is_pubky_not_found(&err) => None,
                    Err(err) => return Err(put_error("read Pubky profile", err)),
                };
                if let Some(raw) = &current {
                    if paykit_lib::content_revision(raw) != *expected_revision {
                        return Err(pubky_profile_changed());
                    }
                    images.extend(profile_image(raw));
                }
                if copy_lock.is_some() {
                    match self
                        .fetch_pubky_profile_raw(public_key, PUBKY_PROFILE_PATH)
                        .await
                    {
                        Ok(Some((raw, _))) => images.extend(profile_image(&raw)),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(%err, "could not read the v0 profile's avatar")
                        }
                    }
                }
                images.dedup();
                let mut avatar_paths = Vec::new();
                for image in &images {
                    avatar_paths.extend(self.own_avatar_paths(public_key, image).await);
                }
                if let Some(copy_lock) = copy_lock {
                    match storage.delete_locked(copy_lock).await {
                        Ok(_) => {}
                        Err(err) if is_pubky_not_found(&err) => {}
                        Err(err) => return Err(put_error("delete Pubky app (v0) profile", err)),
                    }
                }
                if current.is_some() {
                    storage
                        .delete_locked(profile_lock)
                        .await
                        .map_err(|err| put_error("delete Pubky profile", err))?;
                }
                for path in avatar_paths {
                    match storage.delete(path.as_str()).await {
                        Ok(_) => {}
                        Err(err) if is_pubky_not_found(&err) => {}
                        Err(err) => {
                            tracing::warn!(%path, %err, "could not delete Pubky profile avatar")
                        }
                    }
                }
                Ok(())
            })
            .await
        })
        .await
    }

    /// The paths of the avatar `image` names in this identity's own storage: a
    /// social v1 file, or a pubky.app `avatar` file and the blob it points at.
    /// Anything else, or a pubky.app file that cannot be read, gives none.
    async fn own_avatar_paths(&self, public_key: &PubkyPublicKey, image: &str) -> Vec<String> {
        if let Some(path) = own_file_path(public_key, image, SOCIAL_V1_FILE_PATH_PREFIX) {
            return vec![path];
        }
        let Some(file_path) = own_file_path(public_key, image, PUBKY_APP_FILE_PATH_PREFIX) else {
            return Vec::new();
        };
        let file = match self.fetch_pubky_profile_raw(public_key, &file_path).await {
            Ok(Some((raw, _))) => raw,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(%file_path, %err, "could not read the Pubky app avatar file");
                return Vec::new();
            }
        };
        match pubky_app_avatar_blob(&file)
            .and_then(|src| own_file_path(public_key, &src, PUBKY_APP_BLOB_PATH_PREFIX))
        {
            Some(blob_path) => vec![file_path, blob_path],
            None => Vec::new(),
        }
    }

    /// Upload avatar bytes and return the file URI to set as the profile `image`.
    ///
    /// A social v1 identity's avatar is one Pubky social v1 file, the raw bytes
    /// named by their content hash and extension. Otherwise it is a pubky.app
    /// (v0) blob and file: the blob id is the content hash and the file id the
    /// upload time, as `pubky-app-specs` defines them. An identity with no
    /// profile on either layout gets [`PaykitSdkError::Protocol`]: it publishes
    /// its profile first, since Nexus ignores a social v1 file written before
    /// the user's first social v1 profile. The session must hold
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
        let version = own_social_version(&session_access).await?;
        if version == SocialVersion::V0 && !has_pubky_app_data(&session_access).await? {
            return Err(PaykitSdkError::Protocol {
                context: "publish the Pubky app profile before its avatar".into(),
                source: None,
            });
        }
        if version == SocialVersion::V1 {
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

    /// The layout of `public_key`'s profile, when `public_key` is the signed-in
    /// identity and its session may read pubky.app's migration flag.
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

/// The layout this identity writes its profile, avatar and follows in. The
/// session must hold
/// [`PUBKY_APP_PROFILE_SESSION_CAPABILITIES`], so a session that cannot read the
/// flag is signed in again rather than writing a layout pubky.app no longer reads.
async fn own_social_version(session_access: &PubkySessionAccess) -> Result<SocialVersion> {
    session_access
        .validate_for_capabilities(PUBKY_APP_PROFILE_SESSION_CAPABILITIES)
        .map_err(|_| missing_scope())?;
    read_social_version(session_access).await
}

/// Social v1 once pubky.app's migration flag says so, or once the identity has
/// a social v1 profile without it: pubky.app's migrator writes the profile
/// before the flag, a profile Bitkit created has no flag, and Nexus reads the
/// user as social v1 from that profile on.
async fn read_social_version(session_access: &PubkySessionAccess) -> Result<SocialVersion> {
    const CONTEXT: &str = "read Pubky social v1 migration flag";
    let storage = session_access.session.storage();
    let flag = match storage.get(SOCIAL_V1_MIGRATION_FLAG_PATH).await {
        Ok(response) => read_public_response(response, MAX_MIGRATION_FLAG_BYTES, CONTEXT).await?,
        Err(err) if is_pubky_not_found(&err) => None,
        Err(err) => return Err(put_error(CONTEXT, err)),
    };
    if SocialVersion::from_migration_flag(flag.as_deref()) == SocialVersion::V1 {
        return Ok(SocialVersion::V1);
    }
    let has_social_v1_profile = storage
        .exists(SOCIAL_V1_PROFILE_PATH)
        .await
        .map_err(|err| put_error("check Pubky social v1 profile", err))?;
    Ok(if has_social_v1_profile {
        SocialVersion::V1
    } else {
        SocialVersion::V0
    })
}

/// Whether the identity has anything under `/pub/pubky.app/`, which keeps a
/// first profile on pubky.app (v0) until pubky.app migrates it.
async fn has_pubky_app_data(session_access: &PubkySessionAccess) -> Result<bool> {
    const CONTEXT: &str = "list Pubky app (v0) data";
    let entries = session_access
        .session
        .storage()
        .list(PUBKY_APP_PATH_PREFIX)
        .map_err(|err| put_error(CONTEXT, err))?
        .shallow(true)
        .limit(1)
        .send()
        .await;
    match entries {
        Ok(entries) => Ok(!entries.is_empty()),
        Err(err) if is_pubky_not_found(&err) => Ok(false),
        Err(err) => Err(put_error(CONTEXT, err)),
    }
}

/// The `image` URI a stored profile names, if any.
fn profile_image(raw: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()?
        .get("image")?
        .as_str()
        .map(str::to_owned)
}

/// This identity's own file at `uri` directly under `prefix`, else `None`.
/// Only plain names qualify, so the path needs no further encoding.
fn own_file_path(public_key: &PubkyPublicKey, uri: &str, prefix: &str) -> Option<String> {
    let resource = parse_pubky_resource(uri).ok()?;
    let name = resource.path.strip_prefix(prefix)?;
    let plain = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    (resource.public_key == *public_key && plain).then_some(resource.path)
}

/// The blob URI of a pubky.app file object written as a profile avatar.
fn pubky_app_avatar_blob(raw: &[u8]) -> Option<String> {
    let file = serde_json::from_slice::<serde_json::Value>(raw).ok()?;
    (file.get("name")?.as_str()? == AVATAR_FILE_NAME)
        .then(|| file.get("src")?.as_str().map(str::to_owned))?
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    #[test]
    fn own_file_path_takes_only_plain_names_of_this_identity() {
        let me = key();
        let other = key();
        let v1 = SOCIAL_V1_FILE_PATH_PREFIX;
        let uri = |owner: &PubkyPublicKey, path: &str| format!("pubky://{owner}{path}");
        assert_eq!(
            own_file_path(&me, &uri(&me, &format!("{v1}0ABC.jpg")), v1),
            Some(format!("{v1}0ABC.jpg"))
        );
        for rejected in [
            uri(&other, &format!("{v1}0ABC.jpg")),
            uri(&me, &format!("{v1}keep/avatar.jpg")),
            uri(&me, v1),
            uri(&me, &format!("{v1}a%2541")),
            uri(&me, &format!("{v1}%2e%2e")),
            uri(&me, &format!("{v1}..%2Fprofile.json")),
            uri(&me, "/pub/social/v1/profile.json"),
            format!("https://example.com{v1}0ABC.jpg"),
            "not a uri".into(),
        ] {
            assert_eq!(own_file_path(&me, &rejected, v1), None, "{rejected}");
        }
    }

    #[test]
    fn profile_image_and_avatar_blob_read_only_what_they_name() {
        assert_eq!(
            profile_image(br#"{"name":"A","image":"pubky://k/x"}"#),
            Some("pubky://k/x".into())
        );
        assert_eq!(profile_image(br#"{"name":"A"}"#), None);
        assert_eq!(profile_image(b"\xff"), None);
        assert_eq!(
            pubky_app_avatar_blob(br#"{"name":"avatar","src":"pubky://k/b"}"#),
            Some("pubky://k/b".into())
        );
        assert_eq!(
            pubky_app_avatar_blob(br#"{"name":"photo.jpg","src":"pubky://k/b"}"#),
            None
        );
        assert_eq!(pubky_app_avatar_blob(br#"{"name":"avatar"}"#), None);
    }
}
