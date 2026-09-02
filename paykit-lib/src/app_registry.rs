//! Identity-wide Paykit application registry.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
};

use serde::{de, Deserialize, Serialize};

use crate::{
    error::map_error, pubky_routing, validation::invalid_data, PaykitAppId, PaykitError,
    PaymentEndpointIdentifier, PublicKey, Result,
};

const APP_REGISTRY_KIND: &str = "paykit.app_registry";
const APP_REGISTRY_VERSION: u8 = 1;
const APP_DISPLAY_NAME_MAX_LEN: usize = 128;
/// Initial generation for identity-wide Paykit key material.
pub const INITIAL_PAYKIT_KEY_GENERATION: u64 = 1;
/// Maximum serialized Paykit App Registry size accepted from public storage.
pub const PAYKIT_APP_REGISTRY_MAX_BYTES: usize = 64 * 1024;
/// Maximum number of applications in one Paykit App Registry.
pub const PAYKIT_APP_REGISTRY_MAX_APPS: usize = 64;
/// Maximum number of endpoint-specific application defaults in one registry.
pub const PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS: usize = 256;

/// Capabilities advertised by one application in a Paykit identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaykitAppCapabilities {
    /// App can publish or consume private Payment Lists.
    pub private_payments: bool,
    /// App can create or handle Payment Requests.
    pub payment_requests: bool,
    /// App can create or retrieve Receipts.
    pub receipts: bool,
    /// App advertises that it can execute outgoing payments.
    ///
    /// The SDK requires this capability before an App can claim or accept a
    /// Payment Request for execution.
    pub outgoing_payments: bool,
}

/// Public description of one application using a Paykit identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PaykitAppWire", into = "PaykitAppWire")]
pub struct PaykitApp {
    display_name: String,
    capabilities: PaykitAppCapabilities,
}

impl PaykitApp {
    /// Create a validated Paykit app description.
    pub fn new(
        display_name: impl Into<String>,
        capabilities: PaykitAppCapabilities,
    ) -> Result<Self> {
        let display_name = display_name.into();
        validate_display_name(&display_name)?;
        Ok(Self {
            display_name,
            capabilities,
        })
    }

    /// Return the user-readable application name.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Return the application's Paykit capabilities.
    pub fn capabilities(&self) -> PaykitAppCapabilities {
        self.capabilities
    }
}

/// Public registry shared by every application using one Paykit identity.
#[derive(Clone, PartialEq, Eq)]
pub struct PaykitAppRegistry {
    key_generation: u64,
    noise_public_key: Option<PublicKey>,
    apps: HashMap<PaykitAppId, PaykitApp>,
    default_app_id: Option<PaykitAppId>,
    default_apps_by_endpoint: HashMap<PaymentEndpointIdentifier, PaykitAppId>,
}

impl fmt::Debug for PaykitAppRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaykitAppRegistry")
            .field("key_generation", &self.key_generation)
            .field("noise_public_key", &self.noise_public_key)
            .field("apps", &self.apps)
            .field("default_app_id", &self.default_app_id)
            .field("default_apps_by_endpoint", &self.default_apps_by_endpoint)
            .finish()
    }
}

impl PaykitAppRegistry {
    /// Create an empty registry with an optional identity-wide Noise public key.
    ///
    /// Public-only identities can publish applications and Payment Endpoints
    /// before a private-capable application initializes the Noise key.
    pub fn new(noise_public_key: Option<PublicKey>) -> Self {
        Self {
            key_generation: INITIAL_PAYKIT_KEY_GENERATION,
            noise_public_key,
            apps: HashMap::new(),
            default_app_id: None,
            default_apps_by_endpoint: HashMap::new(),
        }
    }

    /// Return the generation of identity-wide Paykit key material.
    pub fn key_generation(&self) -> u64 {
        self.key_generation
    }

    /// Return the identity-wide Noise public key, when initialized.
    pub fn noise_public_key(&self) -> Option<&PublicKey> {
        self.noise_public_key.as_ref()
    }

    /// Initialize or verify the identity-wide Noise public key.
    pub fn set_noise_public_key(&mut self, noise_public_key: PublicKey) -> Result<()> {
        if self
            .noise_public_key
            .as_ref()
            .is_some_and(|existing| existing != &noise_public_key)
        {
            return Err(PaykitError::Validation(
                "Paykit App Registry Noise public key is already initialized to a different key"
                    .into(),
            ));
        }
        self.noise_public_key = Some(noise_public_key);
        Ok(())
    }

    /// Rotate the identity-wide Noise public key to the next key generation.
    pub fn rotate_noise_public_key(
        &mut self,
        noise_public_key: PublicKey,
        key_generation: u64,
    ) -> Result<()> {
        let expected_generation = self.key_generation.checked_add(1).ok_or_else(|| {
            PaykitError::Validation("Paykit App Registry key generation is exhausted".into())
        })?;
        if key_generation != expected_generation {
            return Err(PaykitError::Validation(format!(
                "Paykit App Registry key generation must advance from {} to {expected_generation}",
                self.key_generation
            )));
        }
        self.noise_public_key = Some(noise_public_key);
        self.key_generation = key_generation;
        Ok(())
    }

    /// Return all registered applications keyed by App ID.
    pub fn apps(&self) -> &HashMap<PaykitAppId, PaykitApp> {
        &self.apps
    }

    /// Return the identity-wide default App ID.
    pub fn default_app_id(&self) -> Option<&PaykitAppId> {
        self.default_app_id.as_ref()
    }

    /// Return per-endpoint default applications.
    pub fn default_apps_by_endpoint(&self) -> &HashMap<PaymentEndpointIdentifier, PaykitAppId> {
        &self.default_apps_by_endpoint
    }

    /// Return the preferred application for one Payment Endpoint Identifier.
    ///
    /// An endpoint-specific default takes precedence over the identity-wide
    /// default application.
    pub fn preferred_app_for_endpoint(
        &self,
        identifier: &PaymentEndpointIdentifier,
    ) -> Option<&PaykitAppId> {
        self.default_apps_by_endpoint
            .get(identifier)
            .or(self.default_app_id.as_ref())
    }

    /// Add or replace one application registration.
    pub fn register_app(&mut self, app_id: PaykitAppId, app: PaykitApp) -> Result<()> {
        if self.noise_public_key.is_none() && app_uses_private_protocol(app.capabilities()) {
            return Err(PaykitError::Validation(
                "Paykit App Registry must initialize its Noise public key before registering private capabilities"
                    .into(),
            ));
        }
        if !self.apps.contains_key(&app_id) && self.apps.len() >= PAYKIT_APP_REGISTRY_MAX_APPS {
            return Err(PaykitError::Validation(format!(
                "Paykit App Registry must not contain more than {PAYKIT_APP_REGISTRY_MAX_APPS} applications"
            )));
        }
        self.apps.insert(app_id, app);
        Ok(())
    }

    /// Remove an application and defaults that refer to it.
    pub fn remove_app(&mut self, app_id: &PaykitAppId) -> Option<PaykitApp> {
        let removed = self.apps.remove(app_id)?;
        if self.default_app_id.as_ref() == Some(app_id) {
            self.default_app_id = None;
        }
        self.default_apps_by_endpoint
            .retain(|_, default_app_id| default_app_id != app_id);
        Some(removed)
    }

    /// Set or clear the identity-wide default application.
    pub fn set_default_app(&mut self, app_id: Option<PaykitAppId>) -> Result<()> {
        if let Some(app_id) = &app_id {
            self.require_registered_app(app_id)?;
        }
        self.default_app_id = app_id;
        Ok(())
    }

    /// Set the default application for one Payment Endpoint Identifier.
    pub fn set_default_app_for_endpoint(
        &mut self,
        identifier: PaymentEndpointIdentifier,
        app_id: PaykitAppId,
    ) -> Result<()> {
        self.require_registered_app(&app_id)?;
        if !self.default_apps_by_endpoint.contains_key(&identifier)
            && self.default_apps_by_endpoint.len() >= PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS
        {
            return Err(PaykitError::Validation(format!(
                "Paykit App Registry must not contain more than {PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS} endpoint defaults"
            )));
        }
        self.default_apps_by_endpoint.insert(identifier, app_id);
        Ok(())
    }

    /// Clear the default application for one Payment Endpoint Identifier.
    pub fn clear_default_app_for_endpoint(
        &mut self,
        identifier: &PaymentEndpointIdentifier,
    ) -> Option<PaykitAppId> {
        self.default_apps_by_endpoint.remove(identifier)
    }

    fn require_registered_app(&self, app_id: &PaykitAppId) -> Result<()> {
        if self.apps.contains_key(app_id) {
            return Ok(());
        }
        Err(PaykitError::Validation(format!(
            "Paykit App ID '{app_id}' is not registered"
        )))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaykitAppWire {
    display_name: String,
    capabilities: PaykitAppCapabilities,
}

impl TryFrom<PaykitAppWire> for PaykitApp {
    type Error = PaykitError;

    fn try_from(wire: PaykitAppWire) -> Result<Self> {
        Self::new(wire.display_name, wire.capabilities)
    }
}

impl From<PaykitApp> for PaykitAppWire {
    fn from(app: PaykitApp) -> Self {
        Self {
            display_name: app.display_name,
            capabilities: app.capabilities,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppRegistryWire {
    version: u8,
    kind: String,
    key_generation: u64,
    noise_public_key: Option<String>,
    #[serde(deserialize_with = "deserialize_unique_map")]
    apps: HashMap<String, PaykitAppWire>,
    default_app_id: Option<String>,
    #[serde(deserialize_with = "deserialize_unique_map")]
    default_apps_by_endpoint: HashMap<String, String>,
}

#[derive(Serialize)]
struct AppRegistryWireRef<'a> {
    version: u8,
    kind: &'static str,
    key_generation: u64,
    noise_public_key: Option<String>,
    apps: BTreeMap<&'a str, &'a PaykitApp>,
    default_app_id: Option<&'a str>,
    default_apps_by_endpoint: BTreeMap<&'a str, &'a str>,
}

/// Serialize an identity-wide Paykit App Registry.
pub fn serialize_paykit_app_registry(registry: &PaykitAppRegistry) -> Result<String> {
    validate_registry_limits(registry).map_err(PaykitError::Validation)?;
    let apps = registry
        .apps
        .iter()
        .map(|(app_id, app)| (app_id.as_str(), app))
        .collect();
    let default_apps_by_endpoint = registry
        .default_apps_by_endpoint
        .iter()
        .map(|(identifier, app_id)| (identifier.as_str(), app_id.as_str()))
        .collect();
    let raw_json = serde_json::to_string(&AppRegistryWireRef {
        version: APP_REGISTRY_VERSION,
        kind: APP_REGISTRY_KIND,
        key_generation: registry.key_generation,
        noise_public_key: registry.noise_public_key.as_ref().map(PublicKey::z32),
        apps,
        default_app_id: registry.default_app_id.as_ref().map(PaykitAppId::as_str),
        default_apps_by_endpoint,
    })
    .map_err(|err| {
        PaykitError::Validation(format!("failed to serialize Paykit App Registry: {err}"))
    })?;
    if raw_json.len() > PAYKIT_APP_REGISTRY_MAX_BYTES {
        return Err(PaykitError::Validation(format!(
            "Paykit App Registry must not exceed {PAYKIT_APP_REGISTRY_MAX_BYTES} bytes"
        )));
    }
    Ok(raw_json)
}

/// Parse and validate an identity-wide Paykit App Registry.
pub fn parse_paykit_app_registry_json(raw_json: &str) -> Result<PaykitAppRegistry> {
    if raw_json.len() > PAYKIT_APP_REGISTRY_MAX_BYTES {
        return Err(invalid_data(
            format!("Paykit App Registry exceeds {PAYKIT_APP_REGISTRY_MAX_BYTES} bytes"),
            None,
        ));
    }
    let wire = serde_json::from_str::<AppRegistryWire>(raw_json).map_err(|err| {
        invalid_data(
            format!("Paykit App Registry JSON is invalid: {err}"),
            Some(err.into()),
        )
    })?;
    if wire.version != APP_REGISTRY_VERSION || wire.kind != APP_REGISTRY_KIND {
        return Err(invalid_data(
            format!(
                "unsupported Paykit App Registry version/kind: {}/{}",
                wire.version, wire.kind
            ),
            None,
        ));
    }
    if wire.key_generation == 0 {
        return Err(invalid_data(
            "Paykit App Registry key generation must be greater than zero",
            None,
        ));
    }
    if wire.apps.len() > PAYKIT_APP_REGISTRY_MAX_APPS {
        return Err(invalid_data(
            format!(
                "Paykit App Registry contains more than {PAYKIT_APP_REGISTRY_MAX_APPS} applications"
            ),
            None,
        ));
    }
    if wire.default_apps_by_endpoint.len() > PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS {
        return Err(invalid_data(
            format!(
                "Paykit App Registry contains more than {PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS} endpoint defaults"
            ),
            None,
        ));
    }

    let noise_public_key = wire
        .noise_public_key
        .map(|value| {
            PublicKey::try_from_z32(&value).map_err(|err| {
                invalid_data(
                    format!("Paykit App Registry Noise public key is invalid: {err}"),
                    Some(err.into()),
                )
            })
        })
        .transpose()?;
    let mut registry = PaykitAppRegistry::new(noise_public_key);
    registry.key_generation = wire.key_generation;
    for (raw_app_id, raw_app) in wire.apps {
        let app_id = parse_remote_app_id(raw_app_id)?;
        let app = PaykitApp::try_from(raw_app).map_err(|err| {
            invalid_data(
                format!("Paykit App Registry contains an invalid app: {err}"),
                Some(err.into()),
            )
        })?;
        registry.register_app(app_id, app).map_err(|err| {
            invalid_data(
                format!("Paykit App Registry contains an invalid application: {err}"),
                Some(err.into()),
            )
        })?;
    }

    registry.default_app_id = wire.default_app_id.map(parse_remote_app_id).transpose()?;
    for (raw_identifier, raw_app_id) in wire.default_apps_by_endpoint {
        let identifier = PaymentEndpointIdentifier::new(&raw_identifier).map_err(|err| {
            invalid_data(
                format!(
                    "Paykit App Registry contains invalid Payment Endpoint Identifier '{raw_identifier}'"
                ),
                Some(err.into()),
            )
        })?;
        registry
            .default_apps_by_endpoint
            .insert(identifier, parse_remote_app_id(raw_app_id)?);
    }
    validate_registry_defaults(&registry)?;
    Ok(registry)
}

/// Create the identity-wide Paykit App Registry if it does not exist.
///
/// The caller is responsible for creating a Pubky session with write access to
/// the registry path and for session lifetime, capability scope, and key
/// rotation. Request timeouts are configured on the Pubky client; Paykit does
/// not impose an additional deadline. Concurrent creation fails rather than
/// replacing the existing registry.
///
/// # Errors
///
/// Returns [`PaykitError::Validation`] when the registry cannot be serialized
/// and [`PaykitError::Transport`] when Pubky storage rejects the write.
pub async fn create_paykit_app_registry(
    session: &pubky::PubkySession,
    registry: &PaykitAppRegistry,
) -> Result<()> {
    pubky_routing::create_paykit_app_registry(session, registry)
        .await
        .map_err(|err| map_error("create_paykit_app_registry", err))
}

/// Replace the identity-wide Paykit App Registry if its ETag still matches.
///
/// `etag` must come from the same response as the registry being modified.
/// A concurrent change fails with a Pubky `412 Precondition Failed` transport
/// error so the caller can refetch, merge its mutation, and retry.
/// The caller remains responsible for Pubky session lifetime, capability scope,
/// key rotation, and request timeout configuration.
///
/// # Errors
///
/// Returns [`PaykitError::Validation`] when the registry cannot be serialized
/// and [`PaykitError::Transport`] when the ETag is stale or Pubky rejects the
/// write.
pub async fn update_paykit_app_registry(
    session: &pubky::PubkySession,
    registry: &PaykitAppRegistry,
    etag: &str,
) -> Result<()> {
    pubky_routing::update_paykit_app_registry(session, registry, etag)
        .await
        .map_err(|err| map_error("update_paykit_app_registry", err))
}

/// Fetch the identity-wide Paykit App Registry, if it exists.
///
/// This unauthenticated read returns `Ok(None)` when the registry is missing or
/// empty. The caller chooses the Pubky client, owner identity, timeout policy,
/// and any key-rotation strategy; Paykit only reads the registry path.
///
/// # Errors
///
/// Returns [`PaykitError::InvalidData`] for malformed remote data and
/// [`PaykitError::Transport`] when Pubky storage cannot be read.
pub async fn get_paykit_app_registry(
    storage: &pubky::PublicStorage,
    owner: &PublicKey,
) -> Result<Option<PaykitAppRegistry>> {
    pubky_routing::fetch_paykit_app_registry(storage, owner)
        .await
        .map_err(|err| map_error("get_paykit_app_registry", err))
}

/// Fetch the identity-wide Paykit App Registry and its strong ETag.
///
/// The ETag belongs to the same response as the returned registry and can be
/// passed to [`update_paykit_app_registry`].
///
/// # Errors
///
/// Returns [`PaykitError::InvalidData`] for malformed data or a missing strong
/// ETag and [`PaykitError::Transport`] when Pubky storage cannot be read.
pub async fn get_paykit_app_registry_with_etag(
    storage: &pubky::PublicStorage,
    owner: &PublicKey,
) -> Result<Option<(PaykitAppRegistry, String)>> {
    pubky_routing::fetch_paykit_app_registry_with_etag(storage, owner)
        .await
        .map_err(|err| map_error("get_paykit_app_registry_with_etag", err))
}

fn parse_remote_app_id(value: String) -> Result<PaykitAppId> {
    PaykitAppId::new(&value).map_err(|err| {
        invalid_data(
            format!("Paykit App Registry contains invalid App ID '{value}'"),
            Some(err.into()),
        )
    })
}

fn validate_registry_defaults(registry: &PaykitAppRegistry) -> Result<()> {
    if let Some(app_id) = registry.default_app_id() {
        validate_registered_default(registry, app_id, "identity default")?;
    }
    for (identifier, app_id) in registry.default_apps_by_endpoint() {
        validate_registered_default(
            registry,
            app_id,
            &format!("default for Payment Endpoint Identifier '{identifier}'"),
        )?;
    }
    Ok(())
}

fn validate_registry_limits(registry: &PaykitAppRegistry) -> std::result::Result<(), String> {
    if registry.apps.len() > PAYKIT_APP_REGISTRY_MAX_APPS {
        return Err(format!(
            "Paykit App Registry must not contain more than {PAYKIT_APP_REGISTRY_MAX_APPS} applications"
        ));
    }
    if registry.default_apps_by_endpoint.len() > PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS {
        return Err(format!(
            "Paykit App Registry must not contain more than {PAYKIT_APP_REGISTRY_MAX_ENDPOINT_DEFAULTS} endpoint defaults"
        ));
    }
    if registry.noise_public_key.is_none()
        && registry
            .apps
            .values()
            .any(|app| app_uses_private_protocol(app.capabilities()))
    {
        return Err(
            "Paykit App Registry must initialize its Noise public key before advertising private capabilities"
                .into(),
        );
    }
    Ok(())
}

fn app_uses_private_protocol(capabilities: PaykitAppCapabilities) -> bool {
    capabilities.private_payments || capabilities.payment_requests || capabilities.receipts
}

fn validate_registered_default(
    registry: &PaykitAppRegistry,
    app_id: &PaykitAppId,
    label: &str,
) -> Result<()> {
    if registry.apps.contains_key(app_id) {
        return Ok(());
    }
    Err(invalid_data(
        format!("Paykit App Registry {label} refers to unregistered App ID '{app_id}'"),
        None,
    ))
}

fn validate_display_name(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(PaykitError::Validation(
            "Paykit app display name must not be empty".into(),
        ));
    }
    if value.len() > APP_DISPLAY_NAME_MAX_LEN {
        return Err(PaykitError::Validation(format!(
            "Paykit app display name must not exceed {APP_DISPLAY_NAME_MAX_LEN} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(PaykitError::Validation(
            "Paykit app display name must not contain control characters".into(),
        ));
    }
    Ok(())
}

fn deserialize_unique_map<'de, D, V>(
    deserializer: D,
) -> std::result::Result<HashMap<String, V>, D::Error>
where
    D: de::Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<V>(std::marker::PhantomData<V>);

    impl<'de, V> de::Visitor<'de> for UniqueMapVisitor<V>
    where
        V: Deserialize<'de>,
    {
        type Value = HashMap<String, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object without duplicate keys")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut values = HashMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if values.insert(key.clone(), value).is_some() {
                    return Err(de::Error::custom(format!("duplicate key '{key}'")));
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(std::marker::PhantomData))
}

#[cfg(test)]
mod tests;
