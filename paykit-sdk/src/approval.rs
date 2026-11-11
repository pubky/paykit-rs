//! Paykit's side of approval (Paykit spec 3.1, 3.2): the scopes an app asks for, and the
//! switch permissions `paykit-lib` derives for them, which the signer signs.
//!
//! A certificate lists every scope explicitly, including those another requires: asking for
//! `allowance_execute` also asks for `allowance` and `pay`, and `manage_contacts` for
//! `contacts`. A certificate holds `request` or labelled selling scopes, never both.

use paykit_lib::scopes::{check_requested_scopes, permissions_for, ExtensionPrefix};
use paykit_lib::Label;
use pubky_switch_core::scopes::Permission;

use crate::error::Result;

/// The Paykit scopes an app asks for, built up and checked (Paykit spec 3.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppScopes {
    scopes: Vec<String>,
}

/// The scopes an app's request names, with the switch permissions they need (Paykit spec
/// 3.2), in the order a certificate lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestedScopes {
    /// The scopes, required ones included.
    pub scopes: Vec<String>,
    /// The switch permissions the scopes need, each once, in the certificate's order.
    pub permissions: Vec<Permission>,
}

impl RequestedScopes {
    /// `scopes` and their `permissions` as given, unchecked, such as for scopes of another
    /// protocol on the Switch. [`AppScopes::build`] builds and checks Paykit's.
    pub fn new(scopes: Vec<String>, permissions: Vec<Permission>) -> Self {
        Self {
            scopes,
            permissions,
        }
    }
}

impl AppScopes {
    /// No scopes yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn with(mut self, scope: &str) -> Self {
        if !self.scopes.iter().any(|s| s == scope) {
            self.scopes.push(scope.to_owned());
        }
        self
    }

    /// `pay`: act as payer.
    #[must_use]
    pub fn pay(self) -> Self {
        self.with("pay")
    }

    /// `request`: act as payee, for every label.
    #[must_use]
    pub fn request(self) -> Self {
        self.with("request")
    }

    /// `request:<label>`: act as payee under one label only (Paykit spec 3.1).
    #[must_use]
    pub fn request_label(self, label: &Label) -> Self {
        self.with(&label.scope())
    }

    /// `allowance`: Allowances.
    #[must_use]
    pub fn allowance(self) -> Self {
        self.with("allowance")
    }

    /// `allowance_execute`, with the `allowance` and `pay` it requires.
    #[must_use]
    pub fn allowance_execute(self) -> Self {
        self.with("allowance").with("pay").with("allowance_execute")
    }

    /// `contacts`: read the contacts.
    #[must_use]
    pub fn contacts(self) -> Self {
        self.with("contacts")
    }

    /// `manage_contacts`, with the `contacts` it requires.
    #[must_use]
    pub fn manage_contacts(self) -> Self {
        self.with("contacts").with("manage_contacts")
    }

    /// `ext:<prefix>`: an extension's kinds.
    #[must_use]
    pub fn extension(self, prefix: &ExtensionPrefix) -> Self {
        self.with(&prefix.scope())
    }

    /// The scopes and their permissions, checked as the signer checks an app request (Paykit
    /// wire 3.1): no unknown or malformed name, `request` and labels never together, at most
    /// 8 labels and 8 extension scopes.
    ///
    /// # Errors
    ///
    /// [`crate::PaykitError::Lib`] naming the rule.
    pub fn build(self) -> Result<RequestedScopes> {
        check_requested_scopes(&self.scopes)?;
        Ok(RequestedScopes {
            permissions: permissions_for(&self.scopes),
            scopes: self.scopes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approval_wallet_scopes_and_permissions() {
        let wallet = AppScopes::new().pay().request().build().unwrap();
        assert_eq!(wallet.scopes, vec!["pay".to_owned(), "request".to_owned()]);
        assert_eq!(
            wallet.permissions,
            vec![
                Permission::ClaimsRead,
                Permission::ClaimsWrite,
                Permission::ConversationAdd
            ]
        );
    }

    #[test]
    fn test_approval_adds_required_scopes() {
        let executor = AppScopes::new().allowance_execute().build().unwrap();
        assert!(executor.scopes.contains(&"pay".to_owned()));
        assert!(executor.scopes.contains(&"allowance".to_owned()));
        let contacts = AppScopes::new().manage_contacts().build().unwrap();
        assert_eq!(contacts.scopes, vec!["contacts", "manage_contacts"]);
    }

    #[test]
    fn test_approval_labelled_seller_gets_request_permissions() {
        let label = Label::parse("shop1").unwrap();
        let shop = AppScopes::new().request_label(&label).build().unwrap();
        assert_eq!(shop.scopes, vec!["request:shop1"]);
        assert_eq!(shop.permissions, vec![Permission::ConversationAdd]);
    }

    #[test]
    fn test_approval_refuses_request_beside_a_label() {
        let label = Label::parse("shop1").unwrap();
        assert!(AppScopes::new()
            .request()
            .request_label(&label)
            .build()
            .is_err());
    }

    #[test]
    fn test_approval_refuses_nine_labels() {
        let mut scopes = AppScopes::new();
        for i in 0..9 {
            scopes = scopes.request_label(&Label::parse(&format!("shop{i}")).unwrap());
        }
        assert!(scopes.build().is_err());
    }
}
