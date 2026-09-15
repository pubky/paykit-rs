use serde::{Deserialize, Serialize};
use std::fmt;

use super::{validate_optional_text, PaykitProfile};
use crate::{domain::publication::PublicationStatus, PaykitSdkError, PubkyPublicKey, Result};

const PAYKIT_PUBLIC_CONTACT_PATH_PREFIX: &str = "/pub/paykit/contacts/";
const PUBLIC_CONTACT_KIND: &str = "paykit.contact";
const PUBLIC_CONTACT_VERSION: u32 = 1;
const MAX_LOCAL_LABEL_CHARS: usize = 128;

pub(crate) fn public_contact_path(public_key: &PubkyPublicKey) -> String {
    format!(
        "{PAYKIT_PUBLIC_CONTACT_PATH_PREFIX}{}.json",
        public_key.as_str()
    )
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
    /// Time the Contact Record last changed.
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
struct PublicContactDocument {
    version: u32,
    kind: String,
    public_key: PubkyPublicKey,
}

pub(crate) fn public_contact_json(public_key: &PubkyPublicKey) -> Result<String> {
    serde_json::to_string(&PublicContactDocument {
        version: PUBLIC_CONTACT_VERSION,
        kind: PUBLIC_CONTACT_KIND.into(),
        public_key: public_key.clone(),
    })
    .map_err(|err| PaykitSdkError::Protocol {
        context: format!("failed to serialize Paykit public contact: {err}"),
        source: None,
    })
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
