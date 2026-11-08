//! Paykit's scopes and the switch permissions they need (Paykit spec 3.1, 3.2, 3.5; Paykit
//! wire 3.1).
//!
//! Paykit's scopes are names under the Switch's scope rule (Switch spec 3.3): `pay`,
//! `request`, `request:<label>`, `allowance`, `allowance_execute`, `contacts`,
//! `manage_contacts` and `ext:<prefix>`. The Switch checks every certificate's scopes
//! against its own rule; Paykit's verifiers, at apps and at payers, then check Paykit's rules
//! here and refuse a certificate whose `permissions` aren't exactly the list Paykit's
//! permission table derives from its scopes.
//!
//! Security: these checks decide which app may send, receive and read what. A certificate
//! that fails them is invalid at Paykit's verifiers even though the switch takes it and
//! enforces its signed `permissions` (Paykit wire 3.1).

use pubky_switch_core::records::AppCert;
use pubky_switch_core::scopes::{self as switch_scopes, Permission};
use pubky_switch_core::sig::SignedDoc;

use crate::error::{invalid, Error, Result};

/// The most extension scopes one certificate holds (Paykit spec 3.1).
pub const MAX_EXTENSION_SCOPES: usize = 8;

/// The most labelled selling scopes one certificate holds (Paykit spec 3.1).
pub const MAX_LABELLED_SCOPES: usize = 8;

/// The prefix Paykit keeps for its own kinds, never an extension's (Paykit wire 3.1).
pub const PAYKIT_PREFIX: &str = "paykit";

/// A label of a labelled selling scope `request:<label>` (Paykit spec 3.1): 1 to 32
/// characters of `a-z`, `0-9` and `-`, starting with a letter or digit, the syntax of the
/// Switch's sub-scope. `paykit` is a label like any other.
///
/// Payers and the switch see it, so it should be opaque, like `shop1`, never a name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(String);

impl Label {
    /// Read a label from a document (Paykit spec 3.1, Paykit wire 3.1).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a label outside the syntax.
    pub fn parse(text: &str) -> Result<Self> {
        if !switch_scopes::is_sub(text) {
            return invalid(
                "Paykit wire 3.1: a label is 1 to 32 of a-z, 0-9 and -, starting with a letter or digit",
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// A label the caller supplies.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a label outside the syntax.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The label's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The labelled selling scope `request:<label>`.
    pub fn scope(&self) -> String {
        format!("request:{}", self.0)
    }
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The prefix of an extension scope `ext:<prefix>` and its kinds `<prefix>.<name>` (Paykit
/// spec 3.1, Paykit wire 3.1): 1 to 32 characters of `a-z`, `0-9` and `-`, starting with a
/// letter or digit, and not `paykit`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtensionPrefix(String);

impl ExtensionPrefix {
    /// Read a prefix from a document (Paykit wire 3.1).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a prefix outside the syntax, or `paykit`.
    pub fn parse(text: &str) -> Result<Self> {
        if text == PAYKIT_PREFIX || !switch_scopes::is_sub(text) {
            return invalid(
                "Paykit wire 3.1: an extension prefix is 1 to 32 of a-z, 0-9 and -, starting with a \
                 letter or digit, and not paykit",
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// A prefix the caller supplies.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a prefix outside the syntax, or `paykit`.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The prefix's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The extension scope `ext:<prefix>`.
    pub fn scope(&self) -> String {
        format!("ext:{}", self.0)
    }
}

/// One of Paykit's scopes (Paykit spec 3.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PaykitScope {
    /// `pay`: act as payer.
    Pay,
    /// `request`: act as payee, under every label.
    Request,
    /// `request:<label>`: act as payee under one label only.
    RequestLabel(Label),
    /// `allowance`: Allowance messages and records.
    Allowance,
    /// `allowance_execute`: act as an Allowance executor; requires `allowance` and `pay`.
    AllowanceExecute,
    /// `contacts`: read the contacts, conversation entries and deny list.
    Contacts,
    /// `manage_contacts`: change the contacts and the deny list; requires `contacts`.
    ManageContacts,
    /// `ext:<prefix>`: send and receive the extension kinds `<prefix>.<name>`.
    Extension(ExtensionPrefix),
}

/// What a scope name is to Paykit's verifiers (Paykit wire 3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeName {
    /// One of Paykit's scopes.
    Paykit(PaykitScope),
    /// A name that starts with `ext:` or `request:` but isn't a valid extension or
    /// labelled selling scope, such as `ext:paykit`: a certificate holding one is invalid.
    Malformed,
    /// Any other name, such as `pay:x` or a scope a later version adds: ignored, it grants
    /// no permissions and has no keys.
    Unknown,
}

impl PaykitScope {
    /// The scope's name, as a certificate lists it.
    pub fn name(&self) -> String {
        match self {
            Self::Pay => "pay".into(),
            Self::Request => "request".into(),
            Self::RequestLabel(label) => label.scope(),
            Self::Allowance => "allowance".into(),
            Self::AllowanceExecute => "allowance_execute".into(),
            Self::Contacts => "contacts".into(),
            Self::ManageContacts => "manage_contacts".into(),
            Self::Extension(prefix) => prefix.scope(),
        }
    }

    /// Classify a scope name (Paykit wire 3.1).
    pub fn classify(name: &str) -> ScopeName {
        if let Some(prefix) = name.strip_prefix("ext:") {
            return ExtensionPrefix::parse(prefix)
                .map(|p| ScopeName::Paykit(Self::Extension(p)))
                .unwrap_or(ScopeName::Malformed);
        }
        if let Some(label) = name.strip_prefix("request:") {
            return Label::parse(label)
                .map(|l| ScopeName::Paykit(Self::RequestLabel(l)))
                .unwrap_or(ScopeName::Malformed);
        }
        match name {
            "pay" => ScopeName::Paykit(Self::Pay),
            "request" => ScopeName::Paykit(Self::Request),
            "allowance" => ScopeName::Paykit(Self::Allowance),
            "allowance_execute" => ScopeName::Paykit(Self::AllowanceExecute),
            "contacts" => ScopeName::Paykit(Self::Contacts),
            "manage_contacts" => ScopeName::Paykit(Self::ManageContacts),
            _ => ScopeName::Unknown,
        }
    }

    /// The switch permissions this scope needs, by Paykit's table (Paykit spec 3.2).
    pub fn permissions(&self) -> &'static [Permission] {
        match self {
            Self::Pay => &[
                Permission::ClaimsRead,
                Permission::ClaimsWrite,
                Permission::ConversationAdd,
            ],
            Self::Allowance => &[Permission::ClaimsRead, Permission::ConversationAdd],
            Self::Request | Self::RequestLabel(_) | Self::Extension(_) => {
                &[Permission::ConversationAdd]
            }
            Self::Contacts => &[Permission::ListsRead],
            Self::ManageContacts => &[Permission::ListsManage],
            Self::AllowanceExecute => &[],
        }
    }

    /// Whether the scope has scope keys (Paykit spec 3.5): Paykit's scopes and their labels
    /// do; extension scopes don't.
    pub fn has_keys(&self) -> bool {
        !matches!(self, Self::Extension(_))
    }
}

/// Paykit's scopes among a certificate's names, in order; unknown and malformed names left
/// out.
pub fn paykit_scopes(scopes: &[String]) -> Vec<PaykitScope> {
    scopes
        .iter()
        .filter_map(|s| match PaykitScope::classify(s) {
            ScopeName::Paykit(scope) => Some(scope),
            _ => None,
        })
        .collect()
}

/// Check a certificate's scopes against Paykit's rules (Paykit spec 3.1, Paykit wire 3.1),
/// after the Switch's scope rule (Switch wire 3.1), which every reader applies first and
/// which already refuses a name twice and `request` beside a label.
///
/// Paykit refuses a malformed `ext:` or `request:` name (such as `ext:paykit`), more than 8
/// extension scopes, more than 8 labelled selling scopes, and a scope without the scopes it
/// requires: `allowance_execute` without `allowance` and `pay`, `manage_contacts` without
/// `contacts`. It ignores other names it doesn't know.
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule; the message never repeats a scope.
pub fn check_scopes(scopes: &[String]) -> Result<()> {
    let mut extensions = 0usize;
    let mut labels = 0usize;
    for name in scopes {
        match PaykitScope::classify(name) {
            ScopeName::Malformed => {
                return invalid("Paykit wire 3.1: a malformed extension or labelled selling scope");
            }
            ScopeName::Paykit(PaykitScope::Extension(_)) => extensions += 1,
            ScopeName::Paykit(PaykitScope::RequestLabel(_)) => labels += 1,
            _ => {}
        }
    }
    if extensions > MAX_EXTENSION_SCOPES {
        return invalid("Paykit wire 3.1: more than 8 extension scopes");
    }
    if labels > MAX_LABELLED_SCOPES {
        return invalid("Paykit wire 3.1: more than 8 labelled selling scopes");
    }
    let has = |s: &str| scopes.iter().any(|x| x == s);
    if has("allowance_execute") && !(has("allowance") && has("pay")) {
        return invalid("Paykit spec 3.1: allowance_execute requires allowance and pay");
    }
    if has("manage_contacts") && !has("contacts") {
        return invalid("Paykit spec 3.1: manage_contacts requires contacts");
    }
    Ok(())
}

/// Check the scopes an app asks the signer for (Paykit spec 3.1, Switch wire 3.9): the
/// Switch's scope rule, Paykit's rules, and no name Paykit doesn't know.
///
/// # Errors
///
/// [`Error::Switch`] for the Switch's rule; [`Error::Invalid`] for Paykit's or an unknown
/// name.
pub fn check_requested_scopes(scopes: &[String]) -> Result<()> {
    switch_scopes::check_scopes(scopes)?;
    check_scopes(scopes)?;
    if scopes
        .iter()
        .any(|s| PaykitScope::classify(s) == ScopeName::Unknown)
    {
        return invalid("Paykit spec 3.1: an app request names a scope Paykit doesn't know");
    }
    Ok(())
}

/// The switch permissions a certificate's scopes need (Paykit spec 3.2, Paykit wire 3.1):
/// each permission one of Paykit's scopes grants, once, in the Switch's fixed order. Names
/// Paykit doesn't know, and malformed ones, grant nothing.
pub fn permissions_for(scopes: &[String]) -> Vec<Permission> {
    let granted: Vec<Permission> = paykit_scopes(scopes)
        .iter()
        .flat_map(|s| s.permissions().iter().copied())
        .collect();
    Permission::ALL
        .into_iter()
        .filter(|p| granted.contains(p))
        .collect()
}

/// Paykit's verifiers' checks on a certificate the Switch's checks took (Paykit spec 3.2,
/// Paykit wire 3.1): Paykit's scope rules, and `permissions` exactly the list its scopes
/// need, with none missing and no other.
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule.
pub fn check_certificate(cert: &AppCert) -> Result<()> {
    check_scopes(cert.scopes())?;
    if cert.permissions() != permissions_for(cert.scopes()).as_slice() {
        return invalid(
            "Paykit spec 3.2: permissions aren't exactly the list its Paykit scopes need",
        );
    }
    Ok(())
}

/// Verify a certificate as Paykit's verifiers do: the Switch's checks for `identity`
/// (Switch wire 3.1), then Paykit's ([`check_certificate`]).
///
/// # Errors
///
/// [`Error::Switch`] for the Switch's checks, [`Error::Invalid`] for Paykit's.
pub fn verify_certificate(signed: &SignedDoc, identity: &[u8; 32]) -> Result<AppCert> {
    let cert = AppCert::verify(signed, identity)?;
    check_certificate(&cert)?;
    Ok(cert)
}

/// The scopes among `scopes` that have scope keys (Paykit spec 3.5), in order: Paykit's
/// scopes of spec 3.1 and their labels. Extension scopes have none, and a name Paykit
/// doesn't know, such as `pay:x`, isn't one of its scopes.
pub fn keyed_scopes(scopes: &[String]) -> Vec<String> {
    paykit_scopes(scopes)
        .into_iter()
        .filter(PaykitScope::has_keys)
        .map(|s| s.name())
        .collect()
}

/// Whether `scopes` hold `scope`, holding `name` covering every `name:<sub>` (Switch wire
/// 3.1): plain `request` holds every label.
pub fn holds(scopes: &[String], scope: &PaykitScope) -> bool {
    switch_scopes::holds(scopes, &scope.name())
}

/// Whether `scopes` hold `request` or any labelled selling scope: a labelled selling scope
/// counts as `request` for the kinds it sends and receives (Paykit wire 4.1 check 8).
pub fn acts_as_payee(scopes: &[String]) -> bool {
    scopes.iter().any(|s| {
        s == "request"
            || matches!(
                PaykitScope::classify(s),
                ScopeName::Paykit(PaykitScope::RequestLabel(_))
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(scopes: &[&str]) -> Vec<String> {
        scopes.iter().map(|s| (*s).to_owned()).collect()
    }

    fn names(permissions: &[Permission]) -> Vec<&'static str> {
        permissions.iter().map(|p| p.as_str()).collect()
    }

    #[test]
    fn test_scopes_classify_names() {
        assert_eq!(
            PaykitScope::classify("pay"),
            ScopeName::Paykit(PaykitScope::Pay)
        );
        assert!(matches!(
            PaykitScope::classify("request:shop1"),
            ScopeName::Paykit(PaykitScope::RequestLabel(_))
        ));
        assert!(matches!(
            PaykitScope::classify("request:paykit"),
            ScopeName::Paykit(PaykitScope::RequestLabel(_))
        ));
        assert!(matches!(
            PaykitScope::classify("ext:paykit-cap"),
            ScopeName::Paykit(PaykitScope::Extension(_))
        ));
        for malformed in [
            "ext:paykit",
            "ext:",
            "ext:-x",
            "request:Shop1",
            "request:",
            "request:a_b",
        ] {
            assert_eq!(
                PaykitScope::classify(malformed),
                ScopeName::Malformed,
                "{malformed}"
            );
        }
        for unknown in ["pay:x", "contacts:x", "ext", "future_scope"] {
            assert_eq!(
                PaykitScope::classify(unknown),
                ScopeName::Unknown,
                "{unknown}"
            );
        }
    }

    #[test]
    fn test_scopes_permission_table_rows() {
        for (scopes, want) in [
            (
                vec!["pay"],
                vec!["claims.read", "claims.write", "conversation.add"],
            ),
            (vec!["request"], vec!["conversation.add"]),
            (vec!["request:shop1"], vec!["conversation.add"]),
            (vec!["allowance"], vec!["claims.read", "conversation.add"]),
            (
                vec!["pay", "allowance", "allowance_execute"],
                vec!["claims.read", "claims.write", "conversation.add"],
            ),
            (vec!["contacts"], vec!["lists.read"]),
            (
                vec!["contacts", "manage_contacts"],
                vec!["lists.read", "lists.manage"],
            ),
            (vec!["ext:x"], vec!["conversation.add"]),
            (vec!["future_scope", "pay:x"], vec![]),
            (vec!["ext:paykit"], vec![]),
        ] {
            assert_eq!(names(&permissions_for(&owned(&scopes))), want, "{scopes:?}");
        }
    }

    #[test]
    fn test_scopes_accept_valid_certificates() {
        check_scopes(&owned(&["pay", "contacts", "manage_contacts"])).unwrap();
        check_scopes(&owned(&["pay", "allowance", "allowance_execute"])).unwrap();
        let labels: Vec<String> = (1..=8).map(|i| format!("request:shop{i}")).collect();
        check_scopes(&labels).unwrap();
        let exts: Vec<String> = (1..=8).map(|i| format!("ext:e{i}")).collect();
        check_scopes(&exts).unwrap();
        check_scopes(&owned(&["future_scope"])).unwrap();
    }

    #[test]
    fn test_scopes_reject_each_rule() {
        let labels: Vec<String> = (1..=9).map(|i| format!("request:shop{i}")).collect();
        let exts: Vec<String> = (1..=9).map(|i| format!("ext:e{i}")).collect();
        for (scopes, rule) in [
            (owned(&["ext:paykit"]), "malformed"),
            (owned(&["request:Shop1"]), "malformed"),
            (labels, "more than 8 labelled"),
            (exts, "more than 8 extension"),
            (
                owned(&["allowance_execute", "pay"]),
                "requires allowance and pay",
            ),
            (owned(&["manage_contacts"]), "requires contacts"),
        ] {
            let error = check_scopes(&scopes).unwrap_err().to_string();
            assert!(error.contains(rule), "{scopes:?}: {error}");
        }
    }

    #[test]
    fn test_scopes_requested_refuses_unknown_names_and_request_beside_a_label() {
        check_requested_scopes(&owned(&["request:shop1", "pay"])).unwrap();
        assert!(check_requested_scopes(&owned(&["future_scope"])).is_err());
        assert!(check_requested_scopes(&owned(&["pay:x"])).is_err());
        assert!(check_requested_scopes(&owned(&["request", "request:shop1"])).is_err());
    }

    #[test]
    fn test_scopes_keyed_leave_out_extensions_and_unknown_names() {
        let keyed = keyed_scopes(&owned(&["request", "ext:cap", "pay:x", "allowance"]));
        assert_eq!(keyed, owned(&["request", "allowance"]));
        assert_eq!(
            keyed_scopes(&owned(&["request:shop1"])),
            owned(&["request:shop1"])
        );
    }

    #[test]
    fn test_scopes_request_holds_every_label() {
        let label = PaykitScope::RequestLabel(Label::parse("shop1").unwrap());
        assert!(holds(&owned(&["request"]), &label));
        assert!(holds(&owned(&["request:shop1"]), &label));
        assert!(!holds(&owned(&["request:shop2"]), &label));
        assert!(!holds(&owned(&["request:shop1"]), &PaykitScope::Request));
        assert!(acts_as_payee(&owned(&["request:shop2"])));
        assert!(!acts_as_payee(&owned(&["pay"])));
    }

    #[test]
    fn test_label_and_prefix_syntax() {
        assert!(Label::parse("paykit").is_ok());
        assert!(Label::parse(&"a".repeat(32)).is_ok());
        assert!(Label::parse(&"a".repeat(33)).is_err());
        assert!(ExtensionPrefix::parse("paykit").is_err());
        assert!(ExtensionPrefix::parse("paykit-cap").is_ok());
        assert!(matches!(Label::new("A"), Err(Error::Validation(_))));
    }
}
