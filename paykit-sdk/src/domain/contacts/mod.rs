//! Contact, profile, and contact payment resolution types.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{
    domain::linked_peers::LinkedPeerHandshakeReport,
    domain::outbound_private::OutboundPrivateSendReport,
    domain::private_stream::PrivateStreamIntakeReport, domain::publication::PublicationStatus,
    PaykitSdkError, PaymentAmountContext, PaymentEndpointCandidate, PaymentTarget, PubkyPublicKey,
    Result,
};

/// Default public Paykit profile path.
pub const PAYKIT_PROFILE_PATH: &str = "/pub/paykit/profile.json";
/// Default public Paykit blob prefix.
pub const PAYKIT_PROFILE_BLOB_PATH_PREFIX: &str = "/pub/paykit/blobs/";
/// Default public Paykit contact marker prefix.
pub const PAYKIT_PUBLIC_CONTACT_PATH_PREFIX: &str = "/pub/paykit/contacts/";
/// Pubky app profile path used by read-only fallback/helper APIs.
pub const PUBKY_PROFILE_PATH: &str = "/pub/pubky.app/profile.json";
/// Pubky app follows path used by read-only helper APIs.
pub const PUBKY_FOLLOWS_PATH_PREFIX: &str = "/pub/pubky.app/follows/";

const PROFILE_KIND: &str = "paykit.profile";
const PUBLIC_CONTACT_KIND: &str = "paykit.contact";
const PROFILE_VERSION: u32 = 1;
const PUBLIC_CONTACT_VERSION: u32 = 1;
const MAX_DISPLAY_NAME_CHARS: usize = 128;
const MAX_IMAGE_URI_CHARS: usize = 2048;
const MAX_PROFILE_EXTRA_BYTES: usize = 16 * 1024;
const MAX_PAYKIT_BLOB_NAME_BYTES: usize = 128;
const MAX_LOCAL_LABEL_CHARS: usize = 128;
const MAX_PUBKY_PROFILE_NAME_CHARS: usize = 128;
const MAX_PUBKY_PROFILE_TEXT_CHARS: usize = 4096;
const MAX_PUBKY_PROFILE_URI_CHARS: usize = 2048;
const MAX_PUBKY_PROFILE_LINKS: usize = 32;

/// Public Paykit-facing profile metadata.
///
/// This record is public. Product-specific metadata can be carried in `extra`
/// when it is safe to publish.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitProfile {
    /// Public display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Public image pointer such as a Pubky path or URL.
    #[serde(default)]
    pub image_uri: Option<String>,
    /// App-specific public profile fields.
    #[serde(default)]
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Public profile metadata from the Pubky app namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyProfile {
    /// Public display name.
    pub name: String,
    /// Optional profile bio.
    #[serde(default)]
    pub bio: Option<String>,
    /// Optional public image pointer.
    #[serde(default)]
    pub image: Option<String>,
    /// Optional public profile links.
    #[serde(default)]
    pub links: Option<Vec<PubkyProfileLink>>,
    /// Optional public status text.
    #[serde(default)]
    pub status: Option<String>,
}

impl PubkyProfile {
    /// Validate Pubky profile field bounds.
    pub fn validate(&self) -> Result<()> {
        validate_text(
            &self.name,
            "Pubky profile name",
            MAX_PUBKY_PROFILE_NAME_CHARS,
            false,
        )?;
        validate_optional_text(
            self.bio.as_deref(),
            "Pubky profile bio",
            MAX_PUBKY_PROFILE_TEXT_CHARS,
            true,
        )?;
        validate_optional_text(
            self.image.as_deref(),
            "Pubky profile image",
            MAX_PUBKY_PROFILE_URI_CHARS,
            false,
        )?;
        validate_optional_text(
            self.status.as_deref(),
            "Pubky profile status",
            MAX_PUBKY_PROFILE_TEXT_CHARS,
            true,
        )?;
        if let Some(links) = self.links.as_ref() {
            if links.len() > MAX_PUBKY_PROFILE_LINKS {
                return Err(PaykitSdkError::Protocol(format!(
                    "Pubky profile links must not exceed {MAX_PUBKY_PROFILE_LINKS} entries"
                )));
            }
            for link in links {
                link.validate()?;
            }
        }
        Ok(())
    }
}

/// Public profile link from the Pubky app namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyProfileLink {
    /// Link title.
    pub title: String,
    /// Link URL.
    pub url: String,
}

impl PubkyProfileLink {
    /// Validate Pubky profile link field bounds.
    pub fn validate(&self) -> Result<()> {
        validate_text(
            &self.title,
            "Pubky profile link title",
            MAX_PUBKY_PROFILE_NAME_CHARS,
            true,
        )?;
        validate_text(
            &self.url,
            "Pubky profile link URL",
            MAX_PUBKY_PROFILE_URI_CHARS,
            false,
        )
    }
}

/// Pubky profile record fetched through the SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubkyProfileRecord {
    /// Profile owner.
    pub public_key: PubkyPublicKey,
    /// Public profile metadata.
    pub profile: PubkyProfile,
    /// Pubky path used for the profile.
    pub path: String,
    /// Local observation time.
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// Source used for a resolved contact profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContactProfileSource {
    /// Resolved from the configured Paykit Profile path.
    PaykitProfile,
    /// Resolved from `/pub/pubky.app/profile.json`.
    PubkyProfile,
}

/// Contact display profile resolved by trying Paykit Profile first.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactProfileResolution {
    /// Profile owner.
    pub public_key: PubkyPublicKey,
    /// Source that produced this profile.
    pub source: ContactProfileSource,
    /// Normalized display name for app contact lists.
    pub display_name: Option<String>,
    /// Normalized image pointer for app contact lists.
    pub image_uri: Option<String>,
    /// Paykit Profile payload when the source is Paykit Profile.
    pub paykit_profile: Option<PaykitProfile>,
    /// Pubky Profile payload when the source is Pubky Profile.
    pub pubky_profile: Option<PubkyProfile>,
    /// Local observation time.
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

impl fmt::Debug for ContactProfileResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactProfileResolution")
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

impl ContactProfileResolution {
    pub(crate) fn from_paykit(record: PaykitProfileRecord) -> Self {
        Self {
            public_key: record.public_key,
            source: ContactProfileSource::PaykitProfile,
            display_name: record.profile.display_name.clone(),
            image_uri: record.profile.image_uri.clone(),
            paykit_profile: Some(record.profile),
            pubky_profile: None,
            fetched_at: record.updated_at,
        }
    }

    pub(crate) fn from_pubky(record: PubkyProfileRecord) -> Self {
        Self {
            public_key: record.public_key,
            source: ContactProfileSource::PubkyProfile,
            display_name: Some(record.profile.name.clone()),
            image_uri: record.profile.image.clone(),
            paykit_profile: None,
            pubky_profile: Some(record.profile),
            fetched_at: record.fetched_at,
        }
    }
}

impl PaykitProfile {
    /// Validate profile field bounds.
    pub fn validate(&self) -> Result<()> {
        validate_optional_text(
            self.display_name.as_deref(),
            "profile display name",
            MAX_DISPLAY_NAME_CHARS,
            false,
        )?;
        validate_optional_text(
            self.image_uri.as_deref(),
            "profile image URI",
            MAX_IMAGE_URI_CHARS,
            false,
        )?;
        if let Some(extra) = self.extra.as_ref() {
            let extra_json = serde_json::to_vec(extra).map_err(|err| {
                PaykitSdkError::Protocol(format!("profile extra must be valid JSON: {err}"))
            })?;
            if extra_json.len() > MAX_PROFILE_EXTRA_BYTES {
                return Err(PaykitSdkError::Protocol(format!(
                    "profile extra must not exceed {MAX_PROFILE_EXTRA_BYTES} bytes"
                )));
            }
        }
        Ok(())
    }
}

/// Profile record fetched or published through the SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitProfileRecord {
    /// Profile owner.
    pub public_key: PubkyPublicKey,
    /// Public profile metadata.
    pub profile: PaykitProfile,
    /// Pubky path used for the profile.
    pub path: String,
    /// Local observation/publication time.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Public blob published under the configured Paykit namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitBlobRecord {
    /// Blob owner.
    pub public_key: PubkyPublicKey,
    /// Pubky path used for the blob.
    pub path: String,
    /// Canonical `pubky://` URI for the blob.
    pub uri: String,
    /// Blob size in bytes.
    pub size_bytes: u64,
    /// Local publication time.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Local SDK contact update.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactUpdate {
    /// Contact public key.
    pub public_key: PubkyPublicKey,
    /// Optional local display label.
    pub label: Option<String>,
}

impl fmt::Debug for ContactUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactUpdate")
            .field("public_key", &"<redacted>")
            .field("label", &self.label.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ContactUpdate {
    /// Validate contact update fields.
    pub fn validate(&self) -> Result<()> {
        validate_optional_text(
            self.label.as_deref(),
            "contact label",
            MAX_LOCAL_LABEL_CHARS,
            true,
        )
    }
}

/// Local SDK contact record.
///
/// Contact Records are local/private SDK state by default. Publishing a Public
/// Contact Marker requires explicit runtime policy and a separate method call.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    /// Contact public key.
    pub public_key: PubkyPublicKey,
    /// Optional local display label.
    pub label: Option<String>,
    /// Cached public profile, when fetched.
    pub profile: Option<PaykitProfile>,
    /// Time the cached public profile was fetched.
    pub profile_fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Time the contact was first saved locally.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Time the local contact record last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Public Contact Marker publication state.
    pub public_contact_marker_status: PublicationStatus,
    /// Time the contact was last published publicly by explicit opt-in.
    pub public_contact_published_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Time the public contact marker was last removed.
    pub public_contact_removed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Last public contact marker publication/removal error.
    pub public_contact_last_error: Option<String>,
}

impl fmt::Debug for ContactRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactRecord")
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

impl ContactRecord {
    pub(crate) fn from_update(
        update: ContactUpdate,
        existing: Option<ContactRecord>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let label = normalize_label(update.label);
        match existing {
            Some(mut existing) => {
                existing.label = label;
                existing.updated_at = now;
                existing
            }
            None => Self {
                public_key: update.public_key,
                label,
                profile: None,
                profile_fetched_at: None,
                created_at: now,
                updated_at: now,
                public_contact_marker_status: PublicationStatus::NotPublished,
                public_contact_published_at: None,
                public_contact_removed_at: None,
                public_contact_last_error: None,
            },
        }
    }

    pub(crate) fn with_profile(
        mut self,
        profile: Option<PaykitProfile>,
        fetched_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.profile = profile;
        self.profile_fetched_at = Some(fetched_at);
        self.updated_at = fetched_at;
        self
    }

    pub(crate) fn mark_public_contact_publication_pending(
        mut self,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicationStatus::PendingPublication;
        self.public_contact_last_error = None;
        self.updated_at = updated_at;
        self
    }

    pub(crate) fn mark_public_contact_published(
        mut self,
        published_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicationStatus::Published;
        self.public_contact_published_at = Some(published_at);
        self.public_contact_removed_at = None;
        self.public_contact_last_error = None;
        self.updated_at = published_at;
        self
    }

    pub(crate) fn mark_public_contact_removal_pending(
        mut self,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicationStatus::PendingRemoval;
        self.public_contact_last_error = None;
        self.updated_at = updated_at;
        self
    }

    pub(crate) fn mark_public_contact_removed(
        mut self,
        removed_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicationStatus::Removed;
        self.public_contact_published_at = None;
        self.public_contact_removed_at = Some(removed_at);
        self.public_contact_last_error = None;
        self.updated_at = removed_at;
        self
    }

    pub(crate) fn mark_public_contact_failed(
        mut self,
        error: String,
        failed_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicationStatus::Failed;
        self.public_contact_last_error = Some(error);
        self.updated_at = failed_at;
        self
    }

    pub(crate) fn can_remove_locally(&self) -> bool {
        matches!(
            self.public_contact_marker_status,
            PublicationStatus::NotPublished | PublicationStatus::Removed
        )
    }

    pub(crate) fn may_have_public_marker(&self) -> bool {
        matches!(
            self.public_contact_marker_status,
            PublicationStatus::PendingPublication
                | PublicationStatus::PendingRemoval
                | PublicationStatus::Published
        ) || (self.public_contact_published_at.is_some()
            && self.public_contact_removed_at.is_none())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PaykitProfileDocument {
    version: u32,
    kind: String,
    #[serde(flatten)]
    profile: PaykitProfile,
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicContactDocument {
    version: u32,
    kind: String,
    public_key: PubkyPublicKey,
}

pub(crate) fn profile_json(profile: &PaykitProfile) -> Result<String> {
    profile.validate()?;
    serde_json::to_string(&PaykitProfileDocument {
        version: PROFILE_VERSION,
        kind: PROFILE_KIND.into(),
        profile: profile.clone(),
    })
    .map_err(|err| PaykitSdkError::Protocol(format!("failed to serialize Paykit profile: {err}")))
}

pub(crate) fn parse_profile_json(raw_json: &str) -> Result<PaykitProfile> {
    let document = serde_json::from_str::<PaykitProfileDocument>(raw_json)
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid Paykit profile JSON: {err}")))?;
    if document.version != PROFILE_VERSION {
        return Err(PaykitSdkError::Protocol(format!(
            "unsupported Paykit profile version {}",
            document.version
        )));
    }
    if document.kind != PROFILE_KIND {
        return Err(PaykitSdkError::Protocol(format!(
            "unexpected Paykit profile kind '{}'",
            document.kind
        )));
    }
    document.profile.validate()?;
    Ok(document.profile)
}

pub(crate) fn validate_paykit_blob_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PaykitSdkError::Protocol(
            "Paykit blob name must not be empty".into(),
        ));
    }
    if name.len() > MAX_PAYKIT_BLOB_NAME_BYTES {
        return Err(PaykitSdkError::Protocol(format!(
            "Paykit blob name must not exceed {MAX_PAYKIT_BLOB_NAME_BYTES} bytes"
        )));
    }
    if name == "." || name == ".." {
        return Err(PaykitSdkError::Protocol(
            "Paykit blob name must not be a path traversal segment".into(),
        ));
    }
    if name
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_'))
    {
        return Err(PaykitSdkError::Protocol(
            "Paykit blob name may only contain ASCII letters, digits, '.', '-' and '_'".into(),
        ));
    }
    Ok(())
}

pub(crate) fn paykit_blob_path(blob_prefix: &str, name: &str) -> Result<String> {
    validate_paykit_blob_name(name)?;
    Ok(format!("{blob_prefix}{name}"))
}

pub(crate) fn paykit_blob_uri(public_key: &PubkyPublicKey, path: &str) -> String {
    format!("pubky://{}{}", public_key.as_str(), path)
}

pub(crate) fn paykit_blob_path_from_uri_or_path(
    public_key: &PubkyPublicKey,
    blob_prefix: &str,
    uri_or_path: &str,
) -> Result<String> {
    if uri_or_path.starts_with("pubky://") || uri_or_path.starts_with("pubky") {
        let resource = uri_or_path
            .parse::<pubky::PubkyResource>()
            .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky blob URI: {err}")))?;
        let owner = PubkyPublicKey::from_public_key(&resource.owner);
        if &owner != public_key {
            return Err(PaykitSdkError::Protocol(
                "Paykit blob URI owner does not match local identity".into(),
            ));
        }
        return validate_paykit_blob_path(blob_prefix, resource.path.as_str());
    }
    validate_paykit_blob_path(blob_prefix, uri_or_path)
}

fn validate_paykit_blob_path(blob_prefix: &str, path: &str) -> Result<String> {
    let name = path.strip_prefix(blob_prefix).ok_or_else(|| {
        PaykitSdkError::Protocol("Paykit blob path is outside configured blob prefix".into())
    })?;
    validate_paykit_blob_name(name)?;
    Ok(path.to_owned())
}

pub(crate) fn parse_pubky_profile_json(raw_json: &str) -> Result<PubkyProfile> {
    let profile = serde_json::from_str::<PubkyProfile>(raw_json)
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid Pubky profile JSON: {err}")))?;
    profile.validate()?;
    Ok(profile)
}

pub(crate) fn pubky_follow_keys_from_follow_entries(
    entries: Vec<pubky::PubkyResource>,
) -> Vec<PubkyPublicKey> {
    let mut contacts = entries
        .into_iter()
        .filter_map(|entry| {
            direct_pubky_follow_key(entry.path.as_str())
                .and_then(|value| PubkyPublicKey::new(value.to_owned()).ok())
        })
        .collect::<Vec<_>>();
    contacts.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    contacts.dedup_by(|left, right| left == right);
    contacts
}

fn direct_pubky_follow_key(path: &str) -> Option<&str> {
    let value = path.strip_prefix(PUBKY_FOLLOWS_PATH_PREFIX)?;
    if value.is_empty() || value.contains('/') {
        return None;
    }
    Some(value)
}

pub(crate) fn public_contact_json(public_key: &PubkyPublicKey) -> Result<String> {
    serde_json::to_string(&PublicContactDocument {
        version: PUBLIC_CONTACT_VERSION,
        kind: PUBLIC_CONTACT_KIND.into(),
        public_key: public_key.clone(),
    })
    .map_err(|err| {
        PaykitSdkError::Protocol(format!("failed to serialize Paykit public contact: {err}"))
    })
}

/// Result category for contact payment resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContactPaymentResolutionStatus {
    /// A payable endpoint was found.
    Payable,
    /// No endpoint was found.
    NoEndpoint,
    /// Endpoints exist but are unsupported.
    UnsupportedEndpoint,
}

/// Private-payment state observed while resolving a contact payment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContactPaymentResolutionPrivateState {
    /// Private Payment List candidates were available for resolution.
    Available,
    /// No Private Payment List candidate was available.
    NoPrivateEndpoint,
    /// Private payment state is blocked by link recovery.
    RecoveryPending,
    /// The local identity cannot establish private links.
    PublicOnlySession,
}

/// Request to resolve payable endpoints for one counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPaymentResolutionRequest {
    /// Counterparty to pay.
    pub counterparty: PubkyPublicKey,
    /// Optional amount context used by the payment adapter.
    pub amount: Option<PaymentAmountContext>,
    /// Include public Payment Endpoints after private candidates.
    pub include_public_endpoints: bool,
}

/// Result of preparing a contact payment and resolving payable endpoints.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedContactPayment {
    /// Endpoint resolution after preparation.
    pub resolution: ContactPaymentResolution,
    /// Link handshake/advance report when the SDK attempted private setup.
    pub link_report: Option<LinkedPeerHandshakeReport>,
    /// Private receive report when the SDK refreshed the private stream.
    pub receive_report: Option<PrivateStreamIntakeReport>,
    /// Outbound send report when the SDK processed pending private messages.
    pub outbound_report: Option<OutboundPrivateSendReport>,
    /// Private preparation error when public fallback was allowed.
    pub private_error: Option<String>,
}

impl fmt::Debug for PreparedContactPayment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedContactPayment")
            .field("resolution", &self.resolution)
            .field("link_report", &self.link_report)
            .field("receive_report", &self.receive_report)
            .field("outbound_report", &self.outbound_report)
            .field(
                "private_error",
                &self.private_error.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Payment Endpoint paired with the target needed to pay through it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPaymentEndpoint {
    /// Payable endpoint returned by the payment adapter.
    pub endpoint: PaymentEndpointCandidate,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: PaymentTarget,
}

impl fmt::Debug for ResolvedPaymentEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedPaymentEndpoint")
            .field("endpoint", &self.endpoint)
            .field("target", &self.target)
            .finish()
    }
}

/// Result of resolving contact Payment Endpoints.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPaymentResolution {
    /// General payment resolution outcome.
    pub status: ContactPaymentResolutionStatus,
    /// Private-payment-specific state for this resolution.
    pub private_state: ContactPaymentResolutionPrivateState,
    /// Payable Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<ResolvedPaymentEndpoint>,
}

impl fmt::Debug for ContactPaymentResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactPaymentResolution")
            .field("status", &self.status)
            .field("private_state", &self.private_state)
            .field("payable_endpoints", &self.payable_endpoints)
            .finish()
    }
}

fn validate_optional_text(
    value: Option<&str>,
    label: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if !allow_empty && value.trim().is_empty() {
        return Err(PaykitSdkError::Protocol(format!(
            "{label} must not be empty"
        )));
    }
    if value.chars().count() > max_chars {
        return Err(PaykitSdkError::Protocol(format!(
            "{label} must not exceed {max_chars} characters"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(PaykitSdkError::Protocol(format!(
            "{label} must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_text(value: &str, label: &str, max_chars: usize, allow_empty: bool) -> Result<()> {
    validate_optional_text(Some(value), label, max_chars, allow_empty)
}

fn normalize_label(label: Option<String>) -> Option<String> {
    label.and_then(|label| {
        let label = label.trim();
        if label.is_empty() {
            None
        } else {
            Some(label.to_owned())
        }
    })
}

#[cfg(test)]
mod tests;
