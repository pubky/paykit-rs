//! Host choices for Paykit enrollment and wallet operation (Paykit spec 3.1, 3.2).

use crate::WalletError;
use paykit_lib::{kinds::ExtensionKind, scopes::ExtensionPrefix, Label};
use paykit_sdk::{AppScopes, PaykitConfig, RequestedScopes};

/// The Paykit scopes to ask the signer for, including their required scopes (Paykit spec 3.1).
#[derive(Clone, Default, uniffi::Record)]
pub struct WalletScopes {
    /// Pay requests and make payments without a request (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub pay: bool,
    /// Act as payee for every label (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub request: bool,
    /// Act as payee under these labelled selling scopes (Paykit spec 3.5).
    #[uniffi(default = [])]
    pub request_labels: Vec<String>,
    /// Manage Allowances (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub allowance: bool,
    /// Execute payments under Allowances (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub allowance_execute: bool,
    /// Read contacts (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub contacts: bool,
    /// Manage contacts (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub manage_contacts: bool,
    /// Read and publish recovery reports (Paykit spec 3.1).
    #[uniffi(default = false)]
    pub recovery: bool,
    /// Extension prefixes whose message kinds the app may use (Paykit spec 3.1, 3.3).
    #[uniffi(default = [])]
    pub extensions: Vec<String>,
}

impl TryFrom<WalletScopes> for RequestedScopes {
    type Error = WalletError;

    fn try_from(value: WalletScopes) -> Result<Self, Self::Error> {
        let mut scopes = AppScopes::new();
        if value.pay {
            scopes = scopes.pay();
        }
        if value.request {
            scopes = scopes.request();
        }
        for label in value.request_labels {
            let label = Label::new(&label).map_err(|_| WalletError::invalid("request_labels"))?;
            scopes = scopes.request_label(&label);
        }
        if value.allowance {
            scopes = scopes.allowance();
        }
        if value.allowance_execute {
            scopes = scopes.allowance_execute();
        }
        if value.contacts {
            scopes = scopes.contacts();
        }
        if value.manage_contacts {
            scopes = scopes.manage_contacts();
        }
        if value.recovery {
            scopes = scopes.recovery();
        }
        for prefix in value.extensions {
            let prefix =
                ExtensionPrefix::new(&prefix).map_err(|_| WalletError::invalid("extensions"))?;
            scopes = scopes.extension(&prefix);
        }
        let requested = scopes.build().map_err(|_| WalletError::invalid("scopes"))?;
        if requested.scopes.is_empty() {
            return Err(WalletError::invalid("scopes"));
        }
        Ok(requested)
    }
}

/// Wallet settings used at enrollment, load and restore (Paykit spec 3.3, 7.2, 9).
#[derive(Clone, Default, uniffi::Record)]
pub struct WalletConfig {
    /// Age in seconds at which an open attempt is orphaned, defaulting to the rail's floor (Paykit spec 7.2).
    #[uniffi(default = None)]
    pub orphan_after_s: Option<u64>,
    /// Label under which the wallet sends requests (Paykit spec 3.5).
    #[uniffi(default = None)]
    pub selling_label: Option<String>,
    /// Maximum messages re-sent to one requester per day (Paykit spec 9).
    #[uniffi(default = None)]
    pub resend_budget: Option<u64>,
    /// Extension kinds the wallet accepts within its approved prefixes (Paykit spec 3.3).
    #[uniffi(default = [])]
    pub extension_kinds: Vec<String>,
    /// Whether to publish the signed app record on the Homeserver (Switch spec 4.2).
    #[uniffi(default = None)]
    pub publishes_record: Option<bool>,
}

impl TryFrom<WalletConfig> for PaykitConfig {
    type Error = WalletError;

    fn try_from(value: WalletConfig) -> Result<Self, Self::Error> {
        let mut config = Self::default();
        if let Some(seconds) = value.orphan_after_s {
            config = config.with_orphan_after_s(seconds);
        }
        if let Some(label) = value.selling_label {
            config = config.with_selling_label(
                Label::new(&label).map_err(|_| WalletError::invalid("selling_label"))?,
            );
        }
        if let Some(budget) = value.resend_budget {
            config = config.with_resend_budget(budget);
        }
        if let Some(publishes) = value.publishes_record {
            config.app.publishes_record = publishes;
        }
        let kinds = value
            .extension_kinds
            .into_iter()
            .map(|kind| {
                ExtensionKind::new(&kind).map_err(|_| WalletError::invalid("extension_kinds"))
            })
            .collect::<Result<_, _>>()?;
        Ok(config.with_extension_kinds(kinds))
    }
}
