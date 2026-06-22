use paykit_sdk::{
    ContactProfileResolution, ContactProfileSource, ContactRecord, ContactUpdate, PaykitBlobRecord,
    PaykitProfile, PaykitProfileRecord, PubkyProfile, PubkyProfileLink, PubkyProfileRecord,
    PublicationStatus,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    errors::validation_error,
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
pub enum FfiContactProfileSource {
    /// Resolved from the configured Paykit Profile path.
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

/// Contact display profile resolved by trying Paykit Profile first.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiContactProfileResolution {
    /// Profile owner.
    pub public_key: String,
    /// Source that produced this profile.
    pub source: FfiContactProfileSource,
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

/// Public blob published under the configured Paykit namespace.
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
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiContactUpdate {
    /// Contact public key.
    pub public_key: String,
    /// Optional local display label.
    pub label: Option<String>,
}

/// Local SDK contact record.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
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
    /// Time the local contact record last changed as RFC3339 text.
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

#[uniffi::export]
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

    /// Publish a blob under this identity's Paykit profile namespace.
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

    /// Delete a blob by `pubky://` URI or configured Paykit profile path.
    pub async fn delete_paykit_blob(&self, uri_or_path: String) -> Result<(), PaykitFfiError> {
        self.runtime
            .delete_paykit_blob(&uri_or_path)
            .await
            .map_err(Into::into)
    }

    /// Fetch public Pubky file bytes.
    pub async fn fetch_pubky_file(&self, uri: String) -> Result<Option<Vec<u8>>, PaykitFfiError> {
        self.runtime
            .fetch_pubky_file(&uri)
            .await
            .map_err(Into::into)
    }

    /// Fetch a public Pubky UTF-8 text file.
    pub async fn fetch_pubky_text(&self, uri: String) -> Result<Option<String>, PaykitFfiError> {
        self.runtime
            .fetch_pubky_text(&uri)
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

    /// Fetch public Pubky app follows.
    pub async fn fetch_pubky_follows(
        &self,
        public_key: String,
    ) -> Result<Vec<String>, PaykitFfiError> {
        self.runtime
            .fetch_pubky_follows(parse_public_key(public_key)?)
            .await
            .map(|keys| keys.into_iter().map(|key| app_public_key(&key)).collect())
            .map_err(Into::into)
    }

    /// Resolve display metadata for a contact.
    pub async fn resolve_contact_profile(
        &self,
        public_key: String,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<FfiContactProfileResolution>, PaykitFfiError> {
        self.runtime
            .resolve_contact_profile(parse_public_key(public_key)?, allow_pubky_profile_fallback)
            .await
            .map(|resolution| resolution.map(Into::into))
            .map_err(Into::into)
    }

    /// Resolve public profile metadata, preferring Paykit Profile.
    pub async fn resolve_profile(
        &self,
        public_key: String,
        allow_pubky_profile_fallback: bool,
    ) -> Result<Option<FfiContactProfileResolution>, PaykitFfiError> {
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
    ) -> Result<Option<FfiContactProfileResolution>, PaykitFfiError> {
        self.runtime
            .current_profile(allow_pubky_profile_fallback)
            .await
            .map(|resolution| resolution.map(Into::into))
            .map_err(Into::into)
    }

    /// Save or update a local Contact Record.
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

    /// Return one local Contact Record.
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

    /// Return all local Contact Records.
    pub async fn contact_records(&self) -> Result<Vec<FfiContactRecord>, PaykitFfiError> {
        self.runtime
            .contact_records()
            .await
            .map(|records| records.into_iter().map(Into::into).collect())
            .map_err(Into::into)
    }

    /// Remove a local Contact Record when it has no public marker to clean up.
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

    /// Refresh the cached Paykit Profile for a local Contact Record.
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

    /// Publish a public Contact Marker for a local Contact Record.
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
            extra_json: value
                .extra
                .map(|extra| serde_json::to_string(&extra).expect("JSON object serializes")),
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

impl From<ContactProfileResolution> for FfiContactProfileResolution {
    fn from(value: ContactProfileResolution) -> Self {
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

impl From<ContactProfileSource> for FfiContactProfileSource {
    fn from(value: ContactProfileSource) -> Self {
        match value {
            ContactProfileSource::PaykitProfile => Self::PaykitProfile,
            ContactProfileSource::PubkyProfile => Self::PubkyProfile,
            _ => Self::Unknown,
        }
    }
}

fn parse_profile_extra(raw: &str) -> Result<JsonMap<String, JsonValue>, PaykitFfiError> {
    serde_json::from_str::<JsonMap<String, JsonValue>>(raw)
        .map_err(|err| validation_error(format!("profile extra_json must be a JSON object: {err}")))
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
}
