//! Writes to the identity's own Pubky app profile, following `pubky-app-specs`.

use serde_json::{Map as JsonMap, Value as JsonValue};

use super::{PubkyProfile, PubkyProfileLink};
use crate::{PaykitSdkError, PubkyPublicKey, Result};

/// Pubky app blob path prefix. Blob ids are content hashes.
pub const PUBKY_APP_BLOB_PATH_PREFIX: &str = "/pub/pubky.app/blobs/";
/// Pubky app file path prefix. File ids are timestamps.
pub const PUBKY_APP_FILE_PATH_PREFIX: &str = "/pub/pubky.app/files/";
/// Session capabilities needed to write the Pubky app profile and its avatar.
///
/// Apps that edit the shared Pubky app profile add these to their own session
/// scope. They are not part of the Paykit session scopes.
pub const PUBKY_APP_PROFILE_SESSION_CAPABILITIES: &str =
    "/pub/pubky.app/profile.json:rw,/pub/pubky.app/blobs/:rw,/pub/pubky.app/files/:rw";

const NAME_MIN_CHARS: usize = 3;
const NAME_MAX_CHARS: usize = 50;
const DELETED_NAME: &str = "[DELETED]";
const BIO_MAX_CHARS: usize = 160;
const IMAGE_MAX_CHARS: usize = 300;
const LINKS_MAX_COUNT: usize = 5;
const LINK_TITLE_MAX_CHARS: usize = 100;
const LINK_URL_MAX_CHARS: usize = 300;
const STATUS_MAX_CHARS: usize = 50;
const AVATAR_FILE_NAME: &str = "avatar";

/// Changes to apply to the identity's Pubky app profile.
///
/// `None` keeps the stored value. An empty `bio` or `image`, or empty `links`,
/// removes that field. Fields this type does not name, such as `status` and
/// keys added by other apps, are always kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PubkyProfileUpdate {
    /// New display name.
    pub name: Option<String>,
    /// New bio.
    pub bio: Option<String>,
    /// New image URI, normally a `pubky://<key>/pub/pubky.app/files/<id>` URI.
    pub image: Option<String>,
    /// New links, replacing the stored list.
    pub links: Option<Vec<PubkyProfileLink>>,
}

/// Pubky app profile with the revision of the exact bytes it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PubkyProfileSnapshot {
    /// Profile owner.
    pub public_key: PubkyPublicKey,
    /// Public profile metadata.
    pub profile: PubkyProfile,
    /// Content fingerprint to pass back when publishing an update.
    pub revision: String,
    /// Local observation or publication time.
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// Apply `update` to the stored profile JSON and return the bytes to write.
///
/// `current` is the stored document, or `None` when no profile exists yet, in
/// which case `update` must carry a name.
pub(crate) fn merge_pubky_profile_json(
    current: Option<&[u8]>,
    update: &PubkyProfileUpdate,
) -> Result<Vec<u8>> {
    let mut document = match current {
        Some(bytes) => serde_json::from_slice::<JsonMap<String, JsonValue>>(bytes)
            .map_err(|err| protocol(format!("stored Pubky profile is not a JSON object: {err}")))?,
        None => JsonMap::new(),
    };
    if let Some(name) = &update.name {
        document.insert("name".into(), JsonValue::String(name.trim().to_owned()));
    }
    set_or_remove(&mut document, "bio", update.bio.as_deref());
    set_or_remove(&mut document, "image", update.image.as_deref());
    if let Some(links) = &update.links {
        if links.is_empty() {
            document.remove("links");
        } else {
            let links = links
                .iter()
                .map(|link| {
                    serde_json::json!({ "title": link.title.trim(), "url": link.url.trim() })
                })
                .collect();
            document.insert("links".into(), JsonValue::Array(links));
        }
    }

    let profile = serde_json::from_value::<PubkyProfile>(JsonValue::Object(document.clone()))
        .map_err(|err| protocol(format!("invalid Pubky profile: {err}")))?;
    validate_pubky_profile_update(&profile, update, current.is_none())?;
    serde_json::to_vec(&document).map_err(|err| protocol(format!("encode Pubky profile: {err}")))
}

fn set_or_remove(document: &mut JsonMap<String, JsonValue>, key: &str, value: Option<&str>) {
    match value.map(str::trim) {
        Some("") => {
            document.remove(key);
        }
        Some(value) => {
            document.insert(key.into(), JsonValue::String(value.to_owned()));
        }
        None => {}
    }
}

/// Validate a profile against the `pubky-app-specs` user rules.
pub fn validate_pubky_app_profile(profile: &PubkyProfile) -> Result<()> {
    validate_name(&profile.name)?;
    validate_bio(profile.bio.as_deref())?;
    validate_image(profile.image.as_deref())?;
    validate_links(profile.links.as_deref())?;
    if let Some(status) = &profile.status {
        max_chars(status, STATUS_MAX_CHARS, "status")?;
    }
    Ok(())
}

/// Validate only what `update` writes, plus the name of a profile it creates, so a
/// stored field this app does not edit never blocks its save.
fn validate_pubky_profile_update(
    profile: &PubkyProfile,
    update: &PubkyProfileUpdate,
    creating: bool,
) -> Result<()> {
    if creating || update.name.is_some() {
        validate_name(&profile.name)?;
    }
    if update.bio.is_some() {
        validate_bio(profile.bio.as_deref())?;
    }
    if update.image.is_some() {
        validate_image(profile.image.as_deref())?;
    }
    if update.links.is_some() {
        validate_links(profile.links.as_deref())?;
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    let name_chars = name.chars().count();
    if !(NAME_MIN_CHARS..=NAME_MAX_CHARS).contains(&name_chars)
        || name == DELETED_NAME
        || name.chars().any(char::is_control)
    {
        return Err(protocol(format!(
            "Pubky profile name must be {NAME_MIN_CHARS} to {NAME_MAX_CHARS} characters"
        )));
    }
    Ok(())
}

fn validate_bio(bio: Option<&str>) -> Result<()> {
    bio.map_or(Ok(()), |bio| max_chars(bio, BIO_MAX_CHARS, "bio"))
}

fn validate_image(image: Option<&str>) -> Result<()> {
    image.map_or(Ok(()), |image| uri(image, IMAGE_MAX_CHARS, "image"))
}

fn validate_links(links: Option<&[PubkyProfileLink]>) -> Result<()> {
    let Some(links) = links else {
        return Ok(());
    };
    if links.len() > LINKS_MAX_COUNT {
        return Err(protocol(format!(
            "Pubky profile must not have more than {LINKS_MAX_COUNT} links"
        )));
    }
    for link in links {
        if link.title.trim().is_empty() {
            return Err(protocol(
                "Pubky profile link title must not be empty".into(),
            ));
        }
        max_chars(&link.title, LINK_TITLE_MAX_CHARS, "link title")?;
        uri(&link.url, LINK_URL_MAX_CHARS, "link URL")?;
    }
    Ok(())
}

fn max_chars(value: &str, max: usize, field: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(protocol(format!(
            "Pubky profile {field} must not contain control characters"
        )));
    }
    if value.chars().count() > max {
        return Err(protocol(format!(
            "Pubky profile {field} must not exceed {max} characters"
        )));
    }
    Ok(())
}

fn uri(value: &str, max: usize, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(protocol(format!("Pubky profile {field} must not be empty")));
    }
    max_chars(value, max, field)?;
    url::Url::parse(value)
        .map(|_| ())
        .map_err(|_| protocol(format!("Pubky profile {field} must be a valid URI")))
}

/// Blob id: Crockford Base32 of the first half of the content's BLAKE3 hash.
pub(crate) fn pubky_app_blob_id(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    base32::encode(base32::Alphabet::Crockford, &hash.as_bytes()[..16])
}

/// File id: Crockford Base32 of the big-endian creation time in microseconds.
pub(crate) fn pubky_app_timestamp_id(timestamp_micros: i64) -> String {
    base32::encode(base32::Alphabet::Crockford, &timestamp_micros.to_be_bytes())
}

/// File metadata pointing at an uploaded avatar blob.
pub(crate) fn pubky_app_avatar_file_json(
    blob_uri: &str,
    content_type: &str,
    size: usize,
    created_at_micros: i64,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": AVATAR_FILE_NAME,
        "created_at": created_at_micros,
        "src": blob_uri,
        "content_type": content_type,
        "size": size,
    }))
    .expect("file metadata is plain JSON")
}

fn protocol(context: String) -> PaykitSdkError {
    PaykitSdkError::Protocol {
        context,
        source: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update() -> PubkyProfileUpdate {
        PubkyProfileUpdate::default()
    }

    fn merged(current: &str, update: PubkyProfileUpdate) -> JsonValue {
        serde_json::from_slice(
            &merge_pubky_profile_json(Some(current.as_bytes()), &update).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn test_pubky_profile_merge_keeps_untouched_fields() {
        let current = r#"{"name":"Alice","bio":"Hi","status":"away","links":[{"title":"X","url":"https://x.com/a"}],"theme":"dark"}"#;
        let result = merged(
            current,
            PubkyProfileUpdate {
                name: Some(" Alicia ".into()),
                ..update()
            },
        );
        assert_eq!(
            result,
            serde_json::json!({"name":"Alicia","bio":"Hi","status":"away","links":[{"title":"X","url":"https://x.com/a"}],"theme":"dark"})
        );
    }

    #[test]
    fn test_pubky_profile_merge_removes_cleared_fields() {
        let current = r#"{"name":"Alice","bio":"Hi","image":"pubky://a/pub/pubky.app/files/0","links":[{"title":"X","url":"https://x.com/a"}]}"#;
        let result = merged(
            current,
            PubkyProfileUpdate {
                bio: Some(" ".into()),
                image: Some(String::new()),
                links: Some(vec![]),
                ..update()
            },
        );
        assert_eq!(result, serde_json::json!({"name":"Alice"}));
    }

    #[test]
    fn test_pubky_profile_create_requires_valid_name() {
        assert!(merge_pubky_profile_json(None, &update()).is_err());
        let short = PubkyProfileUpdate {
            name: Some("Al".into()),
            ..update()
        };
        assert!(merge_pubky_profile_json(None, &short).is_err());
        let deleted = PubkyProfileUpdate {
            name: Some(DELETED_NAME.into()),
            ..update()
        };
        assert!(merge_pubky_profile_json(None, &deleted).is_err());
    }

    #[test]
    fn test_pubky_profile_merge_enforces_spec_limits() {
        let long_bio = PubkyProfileUpdate {
            name: Some("Alice".into()),
            bio: Some("b".repeat(BIO_MAX_CHARS + 1)),
            ..update()
        };
        assert!(merge_pubky_profile_json(None, &long_bio).is_err());
        let link = PubkyProfileLink {
            title: "X".into(),
            url: "https://x.com".into(),
        };
        let many_links = PubkyProfileUpdate {
            name: Some("Alice".into()),
            links: Some(vec![link; LINKS_MAX_COUNT + 1]),
            ..update()
        };
        assert!(merge_pubky_profile_json(None, &many_links).is_err());
    }

    #[test]
    fn test_pubky_profile_merge_rejects_control_characters() {
        let bio = PubkyProfileUpdate {
            name: Some("Alice".into()),
            bio: Some("Builder\nMaker".into()),
            ..update()
        };
        assert!(merge_pubky_profile_json(None, &bio).is_err());
        let name = PubkyProfileUpdate {
            name: Some("Al\nice".into()),
            ..update()
        };
        assert!(merge_pubky_profile_json(None, &name).is_err());
    }

    #[test]
    fn test_pubky_profile_merge_ignores_stored_fields_it_does_not_write() {
        let current = r#"{"name":"Alice","bio":"Builder\nMaker","status":"a status longer than the fifty characters pubky.app allows"}"#;
        let result = merged(
            current,
            PubkyProfileUpdate {
                name: Some("Alicia".into()),
                ..update()
            },
        );
        assert_eq!(result["name"], "Alicia");
        assert_eq!(result["bio"], "Builder\nMaker");
    }

    #[test]
    fn test_pubky_app_ids_match_specs_encoding() {
        // The specs encode with the `base32` crate's Crockford alphabet, uppercase and unpadded.
        assert_eq!(pubky_app_timestamp_id(1_727_740_800_000_000).len(), 13);
        assert_eq!(pubky_app_blob_id(b"avatar").len(), 26);
        assert_eq!(pubky_app_timestamp_id(0), "0000000000000");
    }
}
