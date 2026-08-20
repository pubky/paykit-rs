use std::fmt;

use paykit_sdk::{
    ContactRecord, ContactUpdate, PaykitBlobRecord, PaykitProfile, PaykitProfileRecord,
    ProfileResolution, ProfileSource, PubkyProfile, PubkyProfileLink, PubkyProfileRecord,
    PublicationStatus,
};

use crate::{
    json::parse_json_object,
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key},
    PaykitFfiError,
};

/// Local publication state for SDK-managed public data.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPublicationStatus {
    /// No publication is known to exist.
    NotPublished,
    /// Publication was recorded locally before the remote write.
    PendingPublication,
    /// Publication is known to exist.
    Published,
    /// Removal was recorded locally before the remote delete.
    PendingRemoval,
    /// Publication is known to be removed.
    Removed,
    /// Last publication or removal attempt failed.
    Failed,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Source used for a resolved contact profile.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiProfileSource {
    /// Resolved from the identity-wide Paykit Profile path.
    PaykitProfile,
    /// Resolved from the Pubky app profile path.
    PubkyProfile,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Public Paykit-facing profile metadata.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitProfile {
    /// Public display name.
    pub display_name: Option<String>,
    /// Public image pointer such as a Pubky path or URL.
    pub image_uri: Option<String>,
    /// App-specific public profile fields encoded as a JSON object.
    pub extra_json: Option<String>,
}

/// Public profile link from the Pubky app namespace.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyProfileLink {
    /// Link title.
    pub title: String,
    /// Link URL.
    pub url: String,
}

/// Public profile metadata from the Pubky app namespace.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyProfile {
    /// Public display name.
    pub name: String,
    /// Optional profile bio.
    pub bio: Option<String>,
    /// Optional public image pointer.
    pub image: Option<String>,
    /// Public profile links.
    pub links: Vec<FfiPubkyProfileLink>,
    /// Optional public status text.
    pub status: Option<String>,
}

/// Profile record fetched or published through the SDK.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitProfileRecord {
    /// Profile owner.
    pub public_key: String,
    /// Public profile metadata.
    pub profile: FfiPaykitProfile,
    /// Pubky path used for the profile.
    pub path: String,
    /// Local observation/publication time as RFC3339 text.
    pub updated_at: String,
}

/// Public profile record fetched from the Pubky app namespace.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyProfileRecord {
    /// Profile owner.
    pub public_key: String,
    /// Public profile metadata.
    pub profile: FfiPubkyProfile,
    /// Pubky path used for the profile.
    pub path: String,
    /// Local observation time as RFC3339 text.
    pub fetched_at: String,
}

/// Public profile resolved by trying Paykit Profile first.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiProfileResolution {
    /// Profile owner.
    pub public_key: String,
    /// Source that produced this profile.
    pub source: FfiProfileSource,
    /// Normalized display name for app contact lists.
    pub display_name: Option<String>,
    /// Normalized image pointer for app contact lists.
    pub image_uri: Option<String>,
    /// Paykit Profile payload when the source is Paykit Profile.
    pub paykit_profile: Option<FfiPaykitProfile>,
    /// Pubky Profile payload when the source is Pubky Profile.
    pub pubky_profile: Option<FfiPubkyProfile>,
    /// Local observation time as RFC3339 text.
    pub fetched_at: String,
}

impl fmt::Debug for FfiProfileResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiProfileResolution")
            .field("public_key", &"<redacted>")
            .field("source", &self.source)
            .field(
                "display_name",
                &self.display_name.as_ref().map(|_| "<redacted>"),
            )
            .field("image_uri", &self.image_uri.as_ref().map(|_| "<redacted>"))
            .field(
                "paykit_profile",
                &self.paykit_profile.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "pubky_profile",
                &self.pubky_profile.as_ref().map(|_| "<redacted>"),
            )
            .field("fetched_at", &self.fetched_at)
            .finish()
    }
}

/// Public blob published under the identity-wide Paykit namespace.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitBlobRecord {
    /// Blob owner.
    pub public_key: String,
    /// Pubky path used for the blob.
    pub path: String,
    /// Canonical `pubky://` URI for the blob.
    pub uri: String,
    /// Blob size in bytes.
    pub size_bytes: u64,
    /// Local publication time as RFC3339 text.
    pub updated_at: String,
}

/// Local SDK contact update.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiContactUpdate {
    /// Contact public key.
    pub public_key: String,
    /// Optional local display label.
    pub label: Option<String>,
}

impl fmt::Debug for FfiContactUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiContactUpdate")
            .field("public_key", &"<redacted>")
            .field("label", &self.label.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Local SDK contact record.
///
/// Generated platform record descriptions redact the record contents.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiContactRecord {
    /// Contact public key.
    pub public_key: String,
    /// Optional local display label.
    pub label: Option<String>,
    /// Cached public profile, when fetched.
    pub profile: Option<FfiPaykitProfile>,
    /// Time the cached public profile was fetched as RFC3339 text.
    pub profile_fetched_at: Option<String>,
    /// Time the contact was first saved locally as RFC3339 text.
    pub created_at: String,
    /// Time the Contact Record last changed as RFC3339 text.
    pub updated_at: String,
    /// Public Contact Marker publication state.
    pub public_contact_marker_status: FfiPublicationStatus,
    /// Time the contact was last published publicly as RFC3339 text.
    pub public_contact_published_at: Option<String>,
    /// Time the public contact marker was last removed as RFC3339 text.
    pub public_contact_removed_at: Option<String>,
    /// Last public contact marker publication/removal error.
    pub public_contact_last_error: Option<String>,
}

impl fmt::Debug for FfiContactRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiContactRecord")
            .field("public_key", &"<redacted>")
            .field("label", &self.label.as_ref().map(|_| "<redacted>"))
            .field("profile", &self.profile.as_ref().map(|_| "<redacted>"))
            .field("profile_fetched_at", &self.profile_fetched_at)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field(
                "public_contact_marker_status",
                &self.public_contact_marker_status,
            )
            .field(
                "public_contact_published_at",
                &self.public_contact_published_at,
            )
            .field("public_contact_removed_at", &self.public_contact_removed_at)
            .field(
                "public_contact_last_error",
                &self
                    .public_contact_last_error
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Publish this identity's Paykit Profile.
    pub async fn publish_paykit_profile(
        &self,
        profile: FfiPaykitProfile,
    ) -> Result<FfiPaykitProfileRecord, PaykitFfiError> {
        self.runtime
            .publish_paykit_profile(profile.try_into()?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Fetch a public Paykit Profile.
    pub async fn fetch_paykit_profile(
        &self,
        public_key: String,
    ) -> Result<Option<FfiPaykitProfileRecord>, PaykitFfiError> {
        self.runtime
            .fetch_paykit_profile(parse_public_key(public_key)?)
            .await
            .map(|record| record.map(Into::into))
            .map_err(Into::into)
    }

    /// Delete this identity's Paykit Profile.
    pub async fn delete_paykit_profile(&self) -> Result<(), PaykitFfiError> {
        self.runtime
            .delete_paykit_profile()
            .await
            .map_err(Into::into)
    }

    /// Publish a blob under this identity's Paykit blob path.
    pub async fn publish_paykit_blob(
        &self,
        blob_name: String,
        bytes: Vec<u8>,
    ) -> Result<FfiPaykitBlobRecord, PaykitFfiError> {
        self.runtime
            .publish_paykit_blob(blob_name, bytes)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Upload profile avatar bytes and return the published blob record.
    pub async fn upload_profile_avatar(
        &self,
        bytes: Vec<u8>,
        content_type: String,
    ) -> Result<FfiPaykitBlobRecord, PaykitFfiError> {
        self.runtime
            .upload_profile_avatar(bytes, &content_type)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Delete a blob by `pubky://` URI or identity-wide Paykit blob path.
    pub async fn delete_paykit_blob(&self, uri_or_path: String) -> Result<(), PaykitFfiError> {
        self.runtime
            .delete_paykit_blob(&uri_or_path)
            .await
            .map_err(Into::into)
    }

    /// Fetch public Pubky file bytes up to `max_bytes`.
    pub async fn fetch_pubky_file(
        &self,
        uri: String,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, PaykitFfiError> {
        let max_bytes = usize::try_from(max_bytes).map_err(|_| {
            crate::errors::validation_error("Pubky file byte limit is too large for this platform")
        })?;
        self.runtime
            .fetch_pubky_file(&uri, max_bytes)
            .await
            .map_err(Into::into)
    }

    /// Fetch a public Pubky UTF-8 text file up to `max_bytes`.
    pub async fn fetch_pubky_text(
        &self,
        uri: String,
        max_bytes: u64,
    ) -> Result<Option<String>, PaykitFfiError> {
        let max_bytes = usize::try_from(max_bytes).map_err(|_| {
            crate::errors::validation_error("Pubky text byte limit is too large for this platform")
        })?;
        self.runtime
            .fetch_pubky_text(&uri, max_bytes)
            .await
            .map_err(Into::into)
    }

    /// Fetch a public Pubky app profile.
    pub async fn fetch_pubky_profile(
        &self,
        public_key: String,
    ) -> Result<Option<FfiPubkyProfileRecord>, PaykitFfiError> {
        self.runtime
            .fetch_pubky_profile(parse_public_key(public_key)?)
            .await
            .map(|record| record.map(Into::into))
            .map_err(Into::into)
    }

    /// Fetch public Pubky app follows up to `max_entries`.
    pub async fn fetch_pubky_follows(
        &self,
        public_key: String,
        max_entries: u64,
    ) -> Result<Vec<String>, PaykitFfiError> {
        let max_entries = usize::try_from(max_entries).map_err(|_| {
            crate::errors::validation_error(
                "Pubky follows entry limit is too large for this platform",
            )
        })?;
        self.runtime
            .fetch_pubky_follows(parse_public_key(public_key)?, max_entries)
            .await
            .map(|keys| keys.into_iter().map(|key| app_public_key(&key)).collect())
            .map_err(Into::into)
    }

    /// Resolve public profile metadata, preferring Paykit Profile.
    pub async fn resolve_profile(
        &self,
        public_key: String,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<FfiProfileResolution>, PaykitFfiError> {
        self.runtime
            .resolve_profile(parse_public_key(public_key)?, allow_pubky_profile_fallback)
            .await
            .map(|resolution| resolution.map(Into::into))
            .map_err(Into::into)
    }

    /// Resolve this identity's public profile.
    pub async fn current_profile(
        &self,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<FfiProfileResolution>, PaykitFfiError> {
        self.runtime
            .current_profile(allow_pubky_profile_fallback)
            .await
            .map(|resolution| resolution.map(Into::into))
            .map_err(Into::into)
    }

    /// Save or update a Contact Record.
    pub async fn save_contact(
        &self,
        update: FfiContactUpdate,
    ) -> Result<FfiContactRecord, PaykitFfiError> {
        self.runtime
            .save_contact(update.try_into()?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Return one Contact Record.
    pub async fn contact_record(
        &self,
        public_key: String,
    ) -> Result<Option<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .contact_record(&parse_public_key(public_key)?)
            .await
            .map(|record| record.map(Into::into))
            .map_err(Into::into)
    }

    /// Return all Contact Records.
    pub async fn contact_records(&self) -> Result<Vec<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .contact_records()
            .await
            .map(|records| records.into_iter().map(Into::into).collect())
            .map_err(Into::into)
    }

    /// Remove a Contact Record when it has no public marker to clean up.
    pub async fn remove_contact(
        &self,
        public_key: String,
    ) -> Result<Option<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .remove_contact(&parse_public_key(public_key)?)
            .await
            .map(|record| record.map(Into::into))
            .map_err(Into::into)
    }

    /// Refresh the cached Paykit Profile for a Contact Record.
    pub async fn refresh_contact_paykit_profile(
        &self,
        public_key: String,
    ) -> Result<Option<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .refresh_contact_paykit_profile(parse_public_key(public_key)?)
            .await
            .map(|record| record.map(Into::into))
            .map_err(Into::into)
    }

    /// Publish a public Contact Marker for a Contact Record.
    pub async fn publish_public_contact(
        &self,
        public_key: String,
    ) -> Result<FfiContactRecord, PaykitFfiError> {
        self.runtime
            .publish_public_contact(parse_public_key(public_key)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Remove a public Contact Marker.
    pub async fn remove_public_contact(
        &self,
        public_key: String,
    ) -> Result<Option<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .remove_public_contact(parse_public_key(public_key)?)
            .await
            .map(|record| record.map(Into::into))
            .map_err(Into::into)
    }

    /// Retry pending public Contact Marker publication/removal work.
    pub async fn sync_public_contact_markers(
        &self,
    ) -> Result<Vec<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .sync_public_contact_markers()
            .await
            .map(|records| records.into_iter().map(Into::into).collect())
            .map_err(Into::into)
    }
}

impl TryFrom<FfiPaykitProfile> for PaykitProfile {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaykitProfile) -> Result<Self, Self::Error> {
        let extra = value
            .extra_json
            .map(|raw| parse_profile_extra(&raw))
            .transpose()?;
        Ok(Self {
            display_name: value.display_name,
            image_uri: value.image_uri,
            extra,
        })
    }
}

impl From<PaykitProfile> for FfiPaykitProfile {
    fn from(value: PaykitProfile) -> Self {
        Self {
            display_name: value.display_name,
            image_uri: value.image_uri,
            // serde_json::to_string cannot fail for a string-keyed Value map
            // (no arbitrary_precision), so this is defensive: prefer None over a
            // fabricated "{}" that would round-trip into Some(empty map).
            extra_json: value
                .extra
                .and_then(|extra| serde_json::to_string(&extra).ok()),
        }
    }
}

impl From<PubkyProfileLink> for FfiPubkyProfileLink {
    fn from(value: PubkyProfileLink) -> Self {
        Self {
            title: value.title,
            url: value.url,
        }
    }
}

impl From<PubkyProfile> for FfiPubkyProfile {
    fn from(value: PubkyProfile) -> Self {
        Self {
            name: value.name,
            bio: value.bio,
            image: value.image,
            links: value
                .links
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            status: value.status,
        }
    }
}

impl From<PaykitProfileRecord> for FfiPaykitProfileRecord {
    fn from(value: PaykitProfileRecord) -> Self {
        Self {
            public_key: app_public_key(&value.public_key),
            profile: value.profile.into(),
            path: value.path,
            updated_at: value.updated_at.to_rfc3339(),
        }
    }
}

impl From<PubkyProfileRecord> for FfiPubkyProfileRecord {
    fn from(value: PubkyProfileRecord) -> Self {
        Self {
            public_key: app_public_key(&value.public_key),
            profile: value.profile.into(),
            path: value.path,
            fetched_at: value.fetched_at.to_rfc3339(),
        }
    }
}

impl From<ProfileResolution> for FfiProfileResolution {
    fn from(value: ProfileResolution) -> Self {
        Self {
            public_key: app_public_key(&value.public_key),
            source: value.source.into(),
            display_name: value.display_name,
            image_uri: value.image_uri,
            paykit_profile: value.paykit_profile.map(Into::into),
            pubky_profile: value.pubky_profile.map(Into::into),
            fetched_at: value.fetched_at.to_rfc3339(),
        }
    }
}

impl From<PaykitBlobRecord> for FfiPaykitBlobRecord {
    fn from(value: PaykitBlobRecord) -> Self {
        Self {
            public_key: app_public_key(&value.public_key),
            path: value.path,
            uri: value.uri,
            size_bytes: value.size_bytes,
            updated_at: value.updated_at.to_rfc3339(),
        }
    }
}

impl TryFrom<FfiContactUpdate> for ContactUpdate {
    type Error = PaykitFfiError;

    fn try_from(value: FfiContactUpdate) -> Result<Self, Self::Error> {
        Ok(Self {
            public_key: parse_public_key(value.public_key)?,
            label: value.label,
        })
    }
}

impl From<ContactRecord> for FfiContactRecord {
    fn from(value: ContactRecord) -> Self {
        Self {
            public_key: app_public_key(&value.public_key),
            label: value.label,
            profile: value.profile.map(Into::into),
            profile_fetched_at: value.profile_fetched_at.map(|time| time.to_rfc3339()),
            created_at: value.created_at.to_rfc3339(),
            updated_at: value.updated_at.to_rfc3339(),
            public_contact_marker_status: value.public_contact_marker_status.into(),
            public_contact_published_at: value
                .public_contact_published_at
                .map(|time| time.to_rfc3339()),
            public_contact_removed_at: value
                .public_contact_removed_at
                .map(|time| time.to_rfc3339()),
            public_contact_last_error: value.public_contact_last_error,
        }
    }
}

impl From<PublicationStatus> for FfiPublicationStatus {
    fn from(value: PublicationStatus) -> Self {
        match value {
            PublicationStatus::NotPublished => Self::NotPublished,
            PublicationStatus::PendingPublication => Self::PendingPublication,
            PublicationStatus::Published => Self::Published,
            PublicationStatus::PendingRemoval => Self::PendingRemoval,
            PublicationStatus::Removed => Self::Removed,
            PublicationStatus::Failed => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

impl From<ProfileSource> for FfiProfileSource {
    fn from(value: ProfileSource) -> Self {
        match value {
            ProfileSource::PaykitProfile => Self::PaykitProfile,
            ProfileSource::PubkyProfile => Self::PubkyProfile,
            _ => Self::Unknown,
        }
    }
}

fn parse_profile_extra(
    raw: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, PaykitFfiError> {
    parse_json_object("profile extra_json", raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_extra_json_round_trips() {
        let profile = FfiPaykitProfile {
            display_name: Some("Alice".into()),
            image_uri: Some("pubky://alice/avatar.png".into()),
            extra_json: Some(r#"{"app":"bitkit","rank":1}"#.into()),
        };

        let sdk_profile = PaykitProfile::try_from(profile).unwrap();
        assert_eq!(
            sdk_profile
                .extra
                .as_ref()
                .and_then(|extra| extra.get("app"))
                .and_then(|value| value.as_str()),
            Some("bitkit")
        );

        let ffi_profile = FfiPaykitProfile::from(sdk_profile);
        assert!(ffi_profile
            .extra_json
            .as_deref()
            .unwrap()
            .contains("\"app\":\"bitkit\""));
    }

    #[test]
    fn test_profile_extra_json_rejects_non_object() {
        let profile = FfiPaykitProfile {
            display_name: None,
            image_uri: None,
            extra_json: Some(r#"["not","an","object"]"#.into()),
        };

        assert!(matches!(
            PaykitProfile::try_from(profile),
            Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
        ));
    }

    #[test]
    fn test_private_contact_debug_is_redacted() {
        let contact = FfiContactRecord {
            public_key: "pubky-secret-contact".into(),
            label: Some("Private label".into()),
            profile: Some(FfiPaykitProfile {
                display_name: Some("Alice".into()),
                image_uri: None,
                extra_json: None,
            }),
            profile_fetched_at: None,
            created_at: "2026-08-19T00:00:00Z".into(),
            updated_at: "2026-08-19T00:00:00Z".into(),
            public_contact_marker_status: FfiPublicationStatus::NotPublished,
            public_contact_published_at: None,
            public_contact_removed_at: None,
            public_contact_last_error: Some("private failure".into()),
        };

        let debug = format!("{contact:?}");
        assert!(!debug.contains("pubky-secret-contact"));
        assert!(!debug.contains("Private label"));
        assert!(!debug.contains("Alice"));
        assert!(!debug.contains("private failure"));
    }
}
