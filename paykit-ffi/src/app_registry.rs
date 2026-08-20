use std::collections::HashMap;

use paykit_lib::PaymentEndpointIdentifier;
use paykit_sdk::{
    PaykitApp, PaykitAppCapabilities, PaykitAppId, PaykitAppRegistry, PaykitAppRemovalBlockers,
};

use crate::{
    errors::{validation_error, PaykitFfiError},
    sdk::FfiPaykitSdk,
    session::parse_public_key,
};

/// Public capabilities advertised by one Paykit application.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitAppCapabilities {
    /// Application can participate in private Paykit payment workflows.
    pub private_payments: bool,
    /// Application can send or receive Payment Request messages.
    pub payment_requests: bool,
    /// Application can issue or retrieve Paykit Receipts.
    pub receipts: bool,
    /// Application can execute outgoing payments itself.
    pub outgoing_payments: bool,
}

/// One application registered under a Paykit identity.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitApp {
    /// Stable application identifier.
    pub app_id: String,
    /// User-readable application name.
    pub display_name: String,
    /// Public application capabilities.
    pub capabilities: FfiPaykitAppCapabilities,
}

/// Public application registry for one Paykit identity.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitAppRegistry {
    /// Identity-wide Noise public key as raw z32 text.
    ///
    /// This is not a Pubky identity key and must not be passed through Pubky
    /// public-key normalization helpers.
    pub noise_public_key: String,
    /// Registered applications in App ID order.
    pub apps: Vec<FfiPaykitApp>,
    /// Default application for generic payment routing.
    pub default_app_id: Option<String>,
    /// Per-endpoint default applications.
    pub default_apps_by_endpoint: HashMap<String, String>,
}

/// Work that prevents safe removal of this Paykit application.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitAppRemovalBlockers {
    /// Active app-owned Payment Requests.
    pub active_payment_requests: u64,
    /// App-owned private event messages that have not been delivered.
    pub undelivered_private_events: u64,
    /// Receipt issuance records whose Receipt Access was not delivered.
    pub incomplete_receipt_issuances: u64,
    /// Counterparties that still have a non-empty app-owned Private Payment List.
    pub shared_private_payment_lists: u64,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Fetch the public Paykit application registry for an identity.
    pub async fn paykit_app_registry(
        &self,
        public_key: String,
    ) -> Result<Option<FfiPaykitAppRegistry>, PaykitFfiError> {
        self.runtime
            .paykit_app_registry(parse_public_key(public_key)?)
            .await
            .map(|registry| registry.map(Into::into))
            .map_err(Into::into)
    }

    /// Add or replace this application in the identity-wide registry.
    ///
    /// Serialize registry mutations across SDK instances until Pubky supports
    /// conditional registry writes.
    pub async fn publish_paykit_app(
        &self,
        display_name: String,
        capabilities: FfiPaykitAppCapabilities,
    ) -> Result<FfiPaykitAppRegistry, PaykitFfiError> {
        let app = PaykitApp::new(display_name, capabilities.into())
            .map_err(|err| validation_error(err.to_string()))?;
        self.runtime
            .publish_paykit_app(app)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Remove this application's public Payment Endpoints and registry entry.
    ///
    /// Active app-owned Payment Requests, undelivered private events, and
    /// incomplete Receipt issuance must be finished, and shared Private Payment
    /// Lists must be cleared, before removal.
    ///
    /// Serialize registry mutations across SDK instances until Pubky supports
    /// conditional registry writes.
    pub async fn remove_paykit_app(&self) -> Result<FfiPaykitAppRegistry, PaykitFfiError> {
        self.runtime
            .remove_paykit_app()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Report work that must finish before this application can be removed.
    pub async fn paykit_app_removal_blockers(
        &self,
    ) -> Result<FfiPaykitAppRemovalBlockers, PaykitFfiError> {
        self.runtime
            .paykit_app_removal_blockers()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Set or clear the identity-wide default Paykit application.
    ///
    /// Serialize registry mutations across SDK instances until Pubky supports
    /// conditional registry writes.
    pub async fn set_default_paykit_app(
        &self,
        app_id: Option<String>,
    ) -> Result<FfiPaykitAppRegistry, PaykitFfiError> {
        let app_id = app_id
            .map(PaykitAppId::new)
            .transpose()
            .map_err(|err| validation_error(err.to_string()))?;
        self.runtime
            .set_default_paykit_app(app_id)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Set or clear the default Paykit application for one endpoint identifier.
    ///
    /// Serialize registry mutations across SDK instances until Pubky supports
    /// conditional registry writes.
    pub async fn set_default_paykit_app_for_endpoint(
        &self,
        identifier: String,
        app_id: Option<String>,
    ) -> Result<FfiPaykitAppRegistry, PaykitFfiError> {
        let identifier = PaymentEndpointIdentifier::new(&identifier)
            .map_err(|err| validation_error(err.to_string()))?;
        let app_id = app_id
            .map(PaykitAppId::new)
            .transpose()
            .map_err(|err| validation_error(err.to_string()))?;
        self.runtime
            .set_default_paykit_app_for_endpoint(identifier, app_id)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<FfiPaykitAppCapabilities> for PaykitAppCapabilities {
    fn from(value: FfiPaykitAppCapabilities) -> Self {
        Self {
            private_payments: value.private_payments,
            payment_requests: value.payment_requests,
            receipts: value.receipts,
            outgoing_payments: value.outgoing_payments,
        }
    }
}

impl From<PaykitAppCapabilities> for FfiPaykitAppCapabilities {
    fn from(value: PaykitAppCapabilities) -> Self {
        Self {
            private_payments: value.private_payments,
            payment_requests: value.payment_requests,
            receipts: value.receipts,
            outgoing_payments: value.outgoing_payments,
        }
    }
}

impl From<PaykitAppRegistry> for FfiPaykitAppRegistry {
    fn from(value: PaykitAppRegistry) -> Self {
        let mut apps = value
            .apps()
            .iter()
            .map(|(app_id, app)| FfiPaykitApp {
                app_id: app_id.to_string(),
                display_name: app.display_name().to_owned(),
                capabilities: app.capabilities().into(),
            })
            .collect::<Vec<_>>();
        apps.sort_by(|left, right| left.app_id.cmp(&right.app_id));
        Self {
            noise_public_key: value.noise_public_key().z32(),
            apps,
            default_app_id: value.default_app_id().map(ToString::to_string),
            default_apps_by_endpoint: value
                .default_apps_by_endpoint()
                .iter()
                .map(|(identifier, app_id)| (identifier.as_str().to_owned(), app_id.to_string()))
                .collect(),
        }
    }
}

impl From<PaykitAppRemovalBlockers> for FfiPaykitAppRemovalBlockers {
    fn from(value: PaykitAppRemovalBlockers) -> Self {
        Self {
            active_payment_requests: value.active_payment_requests as u64,
            undelivered_private_events: value.undelivered_private_events as u64,
            incomplete_receipt_issuances: value.incomplete_receipt_issuances as u64,
            shared_private_payment_lists: value.shared_private_payment_lists as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_registry_conversion_preserves_apps_and_defaults() {
        let bitkit_id = PaykitAppId::new("bitkit").unwrap();
        let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let capabilities = PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        };
        let mut registry = PaykitAppRegistry::new(pubky::Keypair::random().public_key());
        registry
            .register_app(
                bitkit_id.clone(),
                PaykitApp::new("Bitkit", capabilities).unwrap(),
            )
            .unwrap();
        registry.set_default_app(Some(bitkit_id.clone())).unwrap();
        registry
            .set_default_app_for_endpoint(endpoint.clone(), bitkit_id)
            .unwrap();

        let ffi = FfiPaykitAppRegistry::from(registry);

        assert_eq!(ffi.apps.len(), 1);
        assert_eq!(ffi.apps[0].app_id, "bitkit");
        assert_eq!(ffi.apps[0].display_name, "Bitkit");
        assert_eq!(ffi.apps[0].capabilities, capabilities.into());
        assert_eq!(ffi.default_app_id.as_deref(), Some("bitkit"));
        assert_eq!(
            ffi.default_apps_by_endpoint
                .get(endpoint.as_str())
                .map(String::as_str),
            Some("bitkit")
        );
    }
}
