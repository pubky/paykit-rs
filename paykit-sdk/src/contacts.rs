//! Contact, profile, and contact payment resolution types.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{
    PaykitSdkError, PaymentAmountContext, PaymentEndpointCandidate, PaymentEndpointEvaluation,
    PaymentTarget, PubkyPublicKey, Result,
};

/// Default public Paykit profile path.
pub const PAYKIT_PROFILE_PATH: &str = "/pub/paykit/profile.json";
/// Default public Paykit profile blob prefix.
///
/// Profile image blob upload/delete helpers are caller-managed.
pub const PAYKIT_PROFILE_BLOB_PATH_PREFIX: &str = "/pub/paykit/blobs/";
/// Default public Paykit contact marker prefix.
pub const PAYKIT_PUBLIC_CONTACT_PATH_PREFIX: &str = "/pub/paykit/contacts/";

const PROFILE_KIND: &str = "paykit.profile";
const PUBLIC_CONTACT_KIND: &str = "paykit.contact";
const PROFILE_VERSION: u32 = 1;
const PUBLIC_CONTACT_VERSION: u32 = 1;
const MAX_DISPLAY_NAME_CHARS: usize = 128;
const MAX_IMAGE_URI_CHARS: usize = 2048;
const MAX_LOCAL_LABEL_CHARS: usize = 128;

/// Public Paykit-facing profile metadata.
///
/// This record is intentionally small and public. Keep product-specific profile
/// data outside this SDK profile unless it is safe to publish.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitProfile {
    /// Public display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Public image pointer such as a Pubky path or URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_uri: Option<String>,
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
    pub public_contact_marker_status: PublicContactMarkerStatus,
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

/// Publication state for an optional Public Contact Marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PublicContactMarkerStatus {
    /// No Public Contact Marker is known to be published.
    #[default]
    NotPublished,
    /// Marker publication was recorded locally before the remote write.
    PendingPublication,
    /// Marker is known to be published.
    Published,
    /// Marker removal was recorded locally before the remote delete.
    PendingRemoval,
    /// Marker is known to be removed.
    Removed,
    /// Last marker publication/removal attempt failed.
    Failed,
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
                public_contact_marker_status: PublicContactMarkerStatus::NotPublished,
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
        self.public_contact_marker_status = PublicContactMarkerStatus::PendingPublication;
        self.public_contact_last_error = None;
        self.updated_at = updated_at;
        self
    }

    pub(crate) fn mark_public_contact_published(
        mut self,
        published_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicContactMarkerStatus::Published;
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
        self.public_contact_marker_status = PublicContactMarkerStatus::PendingRemoval;
        self.public_contact_last_error = None;
        self.updated_at = updated_at;
        self
    }

    pub(crate) fn mark_public_contact_removed(
        mut self,
        removed_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.public_contact_marker_status = PublicContactMarkerStatus::Removed;
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
        self.public_contact_marker_status = PublicContactMarkerStatus::Failed;
        self.public_contact_last_error = Some(error);
        self.updated_at = failed_at;
        self
    }

    pub(crate) fn can_remove_locally(&self) -> bool {
        matches!(
            self.public_contact_marker_status,
            PublicContactMarkerStatus::NotPublished | PublicContactMarkerStatus::Removed
        )
    }

    pub(crate) fn may_have_public_marker(&self) -> bool {
        matches!(
            self.public_contact_marker_status,
            PublicContactMarkerStatus::PendingPublication
                | PublicContactMarkerStatus::PendingRemoval
                | PublicContactMarkerStatus::Published
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

pub(crate) fn public_contact_path(public_key: &PubkyPublicKey) -> String {
    format!(
        "{PAYKIT_PUBLIC_CONTACT_PATH_PREFIX}{}.json",
        public_key.as_str()
    )
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
    /// Private recovery is still in progress.
    PrivateRecoveryPending,
    /// The local identity cannot establish private links.
    PublicOnlySession,
}

/// Request to resolve a payable endpoint for one counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPaymentResolutionRequest {
    /// Counterparty to pay.
    pub counterparty: PubkyPublicKey,
    /// Optional amount context used by the payment adapter.
    pub amount: Option<PaymentAmountContext>,
}

/// Result of resolving a contact payment endpoint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPaymentResolution {
    /// Resolution status.
    pub status: ContactPaymentResolutionStatus,
    /// Selected endpoint, when one is payable.
    pub selected_endpoint: Option<PaymentEndpointCandidate>,
    /// Adapter-built payment target for the selected endpoint.
    pub payment_target: Option<PaymentTarget>,
    /// Adapter evaluations from candidate checks.
    pub evaluations: Vec<PaymentEndpointEvaluation>,
    /// Whether public Payment Endpoints were used after private candidates.
    pub used_public_fallback: bool,
}

impl fmt::Debug for ContactPaymentResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactPaymentResolution")
            .field("status", &self.status)
            .field("selected_endpoint", &self.selected_endpoint)
            .field("payment_target", &self.payment_target)
            .field("evaluations", &self.evaluations)
            .field("used_public_fallback", &self.used_public_fallback)
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
