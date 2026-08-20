use serde::{Deserialize, Serialize};
use std::fmt;

use super::{validate_optional_text, validate_text};
use crate::{PaykitSdkError, PubkyPublicKey, Result};

/// Pubky app profile path used by read-only fallback/helper APIs.
pub const PUBKY_PROFILE_PATH: &str = "/pub/pubky.app/profile.json";
/// Pubky app follows path used by read-only helper APIs.
pub const PUBKY_FOLLOWS_PATH_PREFIX: &str = "/pub/pubky.app/follows/";
pub(crate) const PAYKIT_PROFILE_PATH: &str = "/pub/paykit/profile.json";

const PROFILE_KIND: &str = "paykit.profile";
const PROFILE_VERSION: u32 = 1;
const MAX_DISPLAY_NAME_CHARS: usize = 128;
const MAX_IMAGE_URI_CHARS: usize = 2048;
const MAX_PROFILE_EXTRA_BYTES: usize = 16 * 1024;
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
    /// Application-defined public profile fields shared by the identity.
    #[serde(default, with = "crate::json_serde::optional_map")]
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
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Pubky profile links must not exceed {MAX_PUBKY_PROFILE_LINKS} entries"
                    ),
                    source: None,
                });
            }
            for link in links {
                link.validate()?;
            }
        }
        Ok(())
    }

    fn drop_invalid_optional_fields(&mut self) {
        if validate_optional_text(
            self.bio.as_deref(),
            "Pubky profile bio",
            MAX_PUBKY_PROFILE_TEXT_CHARS,
            true,
        )
        .is_err()
        {
            self.bio = None;
        }

        if validate_optional_text(
            self.image.as_deref(),
            "Pubky profile image",
            MAX_PUBKY_PROFILE_URI_CHARS,
            false,
        )
        .is_err()
        {
            self.image = None;
        }

        if validate_optional_text(
            self.status.as_deref(),
            "Pubky profile status",
            MAX_PUBKY_PROFILE_TEXT_CHARS,
            true,
        )
        .is_err()
        {
            self.status = None;
        }

        if let Some(links) = self.links.take() {
            let valid_links = links
                .into_iter()
                .take(MAX_PUBKY_PROFILE_LINKS)
                .filter(|link| link.validate().is_ok())
                .collect::<Vec<_>>();
            self.links = (!valid_links.is_empty()).then_some(valid_links);
        }
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

/// Source used for a resolved profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProfileSource {
    /// Resolved from the identity-wide Paykit Profile path.
    PaykitProfile,
    /// Resolved from `/pub/pubky.app/profile.json`.
    PubkyProfile,
}

/// Public profile resolved by trying Paykit Profile first.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileResolution {
    /// Profile owner.
    pub public_key: PubkyPublicKey,
    /// Source that produced this profile.
    pub source: ProfileSource,
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

impl fmt::Debug for ProfileResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileResolution")
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

impl ProfileResolution {
    pub(crate) fn from_paykit(record: PaykitProfileRecord) -> Self {
        Self {
            public_key: record.public_key,
            source: ProfileSource::PaykitProfile,
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
            source: ProfileSource::PubkyProfile,
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
            let extra_json = serde_json::to_vec(extra).map_err(|err| PaykitSdkError::Protocol {
                context: format!("profile extra must be valid JSON: {err}"),
                source: None,
            })?;
            if extra_json.len() > MAX_PROFILE_EXTRA_BYTES {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "profile extra must not exceed {MAX_PROFILE_EXTRA_BYTES} bytes"
                    ),
                    source: None,
                });
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

#[derive(Debug, Serialize, Deserialize)]
struct PaykitProfileDocument {
    version: u32,
    kind: String,
    #[serde(flatten)]
    profile: PaykitProfile,
}

pub(crate) fn profile_json(profile: &PaykitProfile) -> Result<String> {
    profile.validate()?;
    serde_json::to_string(&PaykitProfileDocument {
        version: PROFILE_VERSION,
        kind: PROFILE_KIND.into(),
        profile: profile.clone(),
    })
    .map_err(|err| PaykitSdkError::Protocol {
        context: format!("failed to serialize Paykit profile: {err}"),
        source: None,
    })
}

pub(crate) fn parse_profile_json(raw_json: &str) -> Result<PaykitProfile> {
    let document = serde_json::from_str::<PaykitProfileDocument>(raw_json).map_err(|err| {
        PaykitSdkError::Protocol {
            context: format!("invalid Paykit profile JSON: {err}"),
            source: None,
        }
    })?;
    if document.version != PROFILE_VERSION {
        return Err(PaykitSdkError::Protocol {
            context: format!("unsupported Paykit profile version {}", document.version),
            source: None,
        });
    }
    if document.kind != PROFILE_KIND {
        return Err(PaykitSdkError::Protocol {
            context: format!("unexpected Paykit profile kind '{}'", document.kind),
            source: None,
        });
    }
    document.profile.validate()?;
    Ok(document.profile)
}

pub(crate) fn parse_pubky_profile_json(raw_json: &str) -> Result<PubkyProfile> {
    let mut profile =
        serde_json::from_str::<PubkyProfile>(raw_json).map_err(|err| PaykitSdkError::Protocol {
            context: format!("invalid Pubky profile JSON: {err}"),
            source: None,
        })?;
    profile.drop_invalid_optional_fields();
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
