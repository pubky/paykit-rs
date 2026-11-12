//! Paykit's scopes and the switch permissions they need (Paykit spec 3.1, 3.2, 3.5; Paykit
//! wire 3.1).
//!
//! Paykit's scopes are names under the Switch's scope rule (Switch spec 3.3): `pay`,
//! `request`, `request:<label>`, `allowance`, `allowance_execute`, `contacts`,
//! `manage_contacts` and `ext:<prefix>`. The Switch checks every certificate's scopes
//! against its own rule; Paykit's verifiers, at apps and at payers, then check Paykit's rules
//! here and the permissions Paykit's permission table derives from its scopes: exactly that
//! list, or, for a certificate that also holds another protocol's scopes, at least it
//! (Switch wire 3.1 "Several protocols", Paykit wire 3.1).
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

/// The prefix of Paykit's own kinds, `paykit.<name>`: Paykit's protocol name, never an
/// extension's, since an extension prefix holds a `-` (Paykit wire 3.1, 4.3).
pub const PAYKIT_PREFIX: &str = "paykit";

/// Paykit's scope roots (Paykit spec 3.1, 3.2): a scope whose root, the part before any `:`,
/// is one of these is Paykit's, valid or not. The roots are reserved for Paykit, so any other
/// well-formed scope is another protocol's (Switch wire 3.1 "Scope roots").
pub const PAYKIT_SCOPE_NAMES: [&str; 7] = [
    "pay",
    "request",
    "allowance",
    "allowance_execute",
    "contacts",
    "manage_contacts",
    "ext",
];

/// The Paykit Profile's path on the Homeserver (Paykit spec 3.4, 4): an app's grant covers it,
/// read and write, only when the user allows that app to edit the Paykit Profile.
pub const PAYKIT_PROFILE_PATH: &str = "/pub/paykit/profile.json";

/// The folder of the Paykit Profile's files on the Homeserver, such as an image its
/// `image_uri` names, as in v0.1 (Paykit spec 3.4): an app's grant covers it, read and write,
/// with the Paykit Profile.
pub const PAYKIT_PROFILE_BLOBS_PATH: &str = "/pub/paykit/blobs/";

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
/// spec 3.1, Paykit wire 3.1, 4.3): 1 to 32 characters of `a-z`, `0-9` and `-`, starting with
/// a letter or digit, and holding a `-`, such as `paykit-cap`.
///
/// A protocol's name, the prefix of its kinds, holds no `-` (Switch wire 3.1 "Scope roots"),
/// so an extension kind is never another protocol's kind, nor Paykit's own `paykit.<name>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtensionPrefix(String);

impl ExtensionPrefix {
    /// Read a prefix from a document (Paykit wire 3.1).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a prefix outside the syntax or without a `-`, such as `paykit`.
    pub fn parse(text: &str) -> Result<Self> {
        if !text.contains('-') || !switch_scopes::is_sub(text) {
            return invalid(
                "Paykit wire 3.1: an extension prefix is 1 to 32 of a-z, 0-9 and -, starting with a \
                 letter or digit, and holds a -",
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// A prefix the caller supplies.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a prefix outside the syntax or without a `-`.
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
    /// `pay`: act as payer; requires `contacts`, so a paying app reads the deny list.
    Pay,
    /// `request`: act as payee, under every label.
    Request,
    /// `request:<label>`: act as payee under one label only.
    RequestLabel(Label),
    /// `allowance`: Allowance messages and records.
    Allowance,
    /// `allowance_execute`: act as an Allowance executor; requires `allowance`, `pay` and
    /// `contacts`, so the executor reads the deny list.
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
    /// labelled selling scope, such as `ext:paykit` or `ext:cap`, whose prefix holds no `-`: a
    /// certificate holding one is invalid.
    Malformed,
    /// A name Paykit gives no meaning under one of its own names, such as `pay:x`,
    /// `contacts:x` or a plain `ext`, or a string that isn't a scope at all: ignored, it
    /// grants no permissions and has no keys.
    Unknown,
    /// A well-formed scope of another protocol: one whose root, the part before any `:`,
    /// isn't one of Paykit's reserved roots ([`PAYKIT_SCOPE_NAMES`]), such as `chat`. It grants
    /// nothing in Paykit and has no keys here; holding one lets the certificate's
    /// `permissions` hold more than Paykit's scopes need (Switch wire 3.1 "Several
    /// protocols").
    OtherProtocol,
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
            _ => match switch_scopes::split(name) {
                Some((base, _)) if !PAYKIT_SCOPE_NAMES.contains(&base) => ScopeName::OtherProtocol,
                _ => ScopeName::Unknown,
            },
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

/// Whether `scopes` hold a scope of another protocol ([`ScopeName::OtherProtocol`]).
pub fn holds_other_protocol(scopes: &[String]) -> bool {
    scopes
        .iter()
        .any(|s| PaykitScope::classify(s) == ScopeName::OtherProtocol)
}

/// Check a certificate's scopes against Paykit's rules (Paykit spec 3.1, Paykit wire 3.1),
/// after the Switch's scope rule (Switch wire 3.1), which every reader applies first and
/// which already refuses a name twice, `request` beside a label, and more than 32 scopes in
/// all.
///
/// Paykit refuses a malformed `ext:` or `request:` name (such as `ext:paykit`), more than 8
/// extension scopes, more than 8 labelled selling scopes, and a scope without the scopes it
/// requires: `pay` without `contacts`, `allowance_execute` without `allowance`, `pay` and
/// `contacts`, and `manage_contacts` without `contacts`. Its bounds count only its own
/// scopes, whatever another protocol's scopes beside them (Switch wire 3.1 "Several
/// protocols"). It ignores other names it doesn't know.
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
    // Paykit spec 3.1: every executor reads the deny list, so it pauses automatic payment
    // to a counterparty on it (Paykit spec 8.1).
    if has("allowance_execute") && !(has("allowance") && has("pay") && has("contacts")) {
        return invalid("Paykit spec 3.1: allowance_execute requires allowance, pay and contacts");
    }
    // Paykit spec 3.1: every paying app reads the deny list, so it refuses to pay a
    // counterparty the user blocked, whichever app pays.
    if has("pay") && !has("contacts") {
        return invalid("Paykit spec 3.1: pay requires contacts");
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
    if scopes.iter().any(|s| {
        matches!(
            PaykitScope::classify(s),
            ScopeName::Unknown | ScopeName::OtherProtocol
        )
    }) {
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
/// Paykit wire 3.1; Switch wire 3.1 "Several protocols"):
///
/// - Paykit's scope rules apply in full to its own scopes ([`check_scopes`]), and its bounds
///   count only them.
/// - Without a scope of another protocol ([`ScopeName::OtherProtocol`]), `permissions` is
///   exactly the list Paykit's scopes need ([`permissions_for`]), with none missing and no
///   other.
/// - With one, such as a wallet that also chats, `permissions` holds every permission
///   Paykit's scopes need, and may hold more: the other protocol's. The Switch's checks have
///   already taken the list as well formed, each a switch permission, once, in order.
///
/// A `pay:x` or a plain `ext` is Paykit's name with no meaning, not another protocol's
/// scope, so it doesn't widen the list. Another protocol's scopes, and the permissions they
/// bring, authorize nothing in Paykit: each of its messages and documents still needs its
/// own Paykit scope (Paykit spec 3.3).
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule.
pub fn check_certificate(cert: &AppCert) -> Result<()> {
    check_scopes(cert.scopes())?;
    let needed = permissions_for(cert.scopes());
    if holds_other_protocol(cert.scopes()) {
        if !needed.iter().all(|p| cert.permissions().contains(p)) {
            return invalid("Paykit spec 3.2: permissions lack one that its Paykit scopes need");
        }
    } else if cert.permissions() != needed.as_slice() {
        return invalid("Paykit spec 3.2: permissions aren't exactly what its Paykit scopes need");
    }
    Ok(())
}

/// Whether `scopes` hold at least one of Paykit's scopes: an app with none, such as one of
/// another protocol only, isn't a Paykit App and sends or receives no Paykit kind (Paykit
/// spec 3.3).
pub fn holds_paykit_scope(scopes: &[String]) -> bool {
    !paykit_scopes(scopes).is_empty()
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
    use pubky_switch_core::keys::{AppKeys, IdentityKey};
    use pubky_switch_core::records::AppCertBody;

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
            "ext:cap",
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
        for unknown in ["pay:x", "contacts:x", "ext", "Chat", "chat:Room"] {
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
            (vec!["ext:x-1"], vec!["conversation.add"]),
            (vec!["ext:cap"], vec![]),
            (vec!["future_scope", "pay:x"], vec![]),
            (vec!["chat", "chat:room1"], vec![]),
            (vec!["ext:paykit"], vec![]),
        ] {
            assert_eq!(names(&permissions_for(&owned(&scopes))), want, "{scopes:?}");
        }
    }

    #[test]
    fn test_scopes_accept_valid_certificates() {
        check_scopes(&owned(&["pay", "contacts"])).unwrap();
        check_scopes(&owned(&["pay", "allowance", "contacts"])).unwrap();
        check_scopes(&owned(&["pay", "contacts", "manage_contacts"])).unwrap();
        check_scopes(&owned(&[
            "pay",
            "allowance",
            "allowance_execute",
            "contacts",
        ]))
        .unwrap();
        let labels: Vec<String> = (1..=8).map(|i| format!("request:shop{i}")).collect();
        check_scopes(&labels).unwrap();
        let exts: Vec<String> = (1..=8).map(|i| format!("ext:e-{i}")).collect();
        check_scopes(&exts).unwrap();
        check_scopes(&owned(&["future_scope"])).unwrap();
    }

    #[test]
    fn test_scopes_reject_each_rule() {
        let labels: Vec<String> = (1..=9).map(|i| format!("request:shop{i}")).collect();
        let exts: Vec<String> = (1..=9).map(|i| format!("ext:e-{i}")).collect();
        for (scopes, rule) in [
            (owned(&["ext:paykit"]), "malformed"),
            (owned(&["ext:cap"]), "malformed"),
            (owned(&["request:Shop1"]), "malformed"),
            (labels, "more than 8 labelled"),
            (exts, "more than 8 extension"),
            (
                owned(&["allowance_execute", "pay", "contacts"]),
                "requires allowance, pay and contacts",
            ),
            (
                owned(&["allowance_execute", "allowance", "contacts"]),
                "requires allowance, pay and contacts",
            ),
            (
                owned(&["allowance_execute", "allowance", "pay"]),
                "requires allowance, pay and contacts",
            ),
            (owned(&["pay"]), "pay requires contacts"),
            (owned(&["pay", "allowance"]), "pay requires contacts"),
            (owned(&["pay", "request"]), "pay requires contacts"),
            (owned(&["pay", "manage_contacts"]), "pay requires contacts"),
            (owned(&["manage_contacts"]), "requires contacts"),
        ] {
            let error = check_scopes(&scopes).unwrap_err().to_string();
            assert!(error.contains(rule), "{scopes:?}: {error}");
        }
    }

    #[test]
    fn test_scopes_requested_refuses_unknown_names_and_request_beside_a_label() {
        check_requested_scopes(&owned(&["request:shop1", "pay", "contacts"])).unwrap();
        assert!(check_requested_scopes(&owned(&["future_scope"])).is_err());
        assert!(check_requested_scopes(&owned(&["pay:x"])).is_err());
        assert!(check_requested_scopes(&owned(&["request", "request:shop1"])).is_err());
    }

    #[test]
    fn test_scopes_keyed_leave_out_extensions_and_unknown_names() {
        let keyed = keyed_scopes(&owned(&["request", "ext:paykit-cap", "pay:x", "allowance"]));
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

    fn cert(scopes: &[&str], permissions: &[Permission]) -> AppCert {
        let identity = IdentityKey::from_secret([6; 32]);
        let app = AppKeys::from_secrets([4; 32], [5; 32]);
        AppCert::issue(
            &identity,
            AppCertBody::new(
                identity.public(),
                app.signing_public(),
                app.bundle_public(),
                owned(scopes),
                permissions,
                1_767_225_600_000,
                1_767_225_600_000 + 86_400_000,
            ),
        )
        .unwrap()
    }

    const LISTS_READ: Permission = Permission::ListsRead;
    const ADD: Permission = Permission::ConversationAdd;

    #[test]
    fn test_scopes_classify_another_protocols_scopes() {
        for other in ["chat", "chat:room1", "future_scope", "a-b_c"] {
            assert_eq!(
                PaykitScope::classify(other),
                ScopeName::OtherProtocol,
                "{other}"
            );
        }
        // Paykit's own names with no meaning, and what isn't a scope, aren't another
        // protocol's.
        for not_other in [
            "pay:x",
            "allowance:x",
            "ext",
            "Chat",
            "chat:",
            "chat:a_b",
            "pay",
        ] {
            assert_ne!(
                PaykitScope::classify(not_other),
                ScopeName::OtherProtocol,
                "{not_other}"
            );
        }
        assert!(holds_other_protocol(&owned(&["pay", "chat"])));
        assert!(!holds_other_protocol(&owned(&["pay", "pay:x", "ext"])));
        assert!(holds_paykit_scope(&owned(&["chat", "ext:paykit-cap"])));
        assert!(!holds_paykit_scope(&owned(&["chat", "pay:x", "ext"])));
    }

    #[test]
    fn test_scopes_certificate_with_another_protocol_holding_paykit_permissions_accepted() {
        // A payee that also chats: Paykit's conversation.add, and the chat's lists.read.
        check_certificate(&cert(&["request", "chat"], &[LISTS_READ, ADD])).unwrap();
        // Exactly Paykit's list is still enough.
        check_certificate(&cert(&["request", "chat"], &[ADD])).unwrap();
    }

    #[test]
    fn test_scopes_certificate_with_another_protocol_missing_a_paykit_permission_refused() {
        let error = check_certificate(&cert(&["request", "chat"], &[LISTS_READ]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("lack one that its Paykit scopes need"),
            "{error}"
        );
        // `claims.write`, which `pay` needs, is missing.
        let error = check_certificate(&cert(
            &["pay", "contacts", "chat"],
            &[Permission::ClaimsRead, LISTS_READ, ADD],
        ))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("lack one that its Paykit scopes need"),
            "{error}"
        );
    }

    #[test]
    fn test_scopes_paykit_only_certificate_with_an_extra_permission_refused() {
        let error = check_certificate(&cert(&["request"], &[LISTS_READ, ADD]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exactly what"), "{error}");
        // `pay:x` and a plain `ext` are Paykit's names, not another protocol's: they don't
        // let the list hold more.
        assert!(check_certificate(&cert(&["request", "pay:x"], &[LISTS_READ, ADD])).is_err());
        assert!(check_certificate(&cert(&["request", "ext"], &[LISTS_READ, ADD])).is_err());
        check_certificate(&cert(&["request", "pay:x", "ext"], &[ADD])).unwrap();
    }

    #[test]
    fn test_scopes_other_protocol_only_certificate_valid() {
        check_certificate(&cert(&["chat"], &[LISTS_READ])).unwrap();
        check_certificate(&cert(&["chat"], &[])).unwrap();
        assert!(!holds_paykit_scope(cert(&["chat"], &[LISTS_READ]).scopes()));
    }

    #[test]
    fn test_scopes_paykit_rules_hold_beside_another_protocol() {
        let mut labels: Vec<String> = (1..=9).map(|i| format!("request:shop{i}")).collect();
        labels.push("chat".into());
        let names: Vec<&str> = labels.iter().map(String::as_str).collect();
        let error = check_certificate(&cert(&names, &[LISTS_READ, ADD]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 8 labelled"), "{error}");
        for (scopes, rule) in [
            (vec!["ext:paykit", "chat"], "malformed"),
            (vec!["ext:cap", "chat"], "malformed"),
            (
                vec!["allowance_execute", "allowance", "chat"],
                "requires allowance, pay and contacts",
            ),
            (
                vec!["allowance_execute", "allowance", "pay", "chat"],
                "requires allowance, pay and contacts",
            ),
            (vec!["pay", "chat"], "pay requires contacts"),
            (vec!["pay", "allowance", "chat"], "pay requires contacts"),
            (vec!["manage_contacts", "chat"], "requires contacts"),
        ] {
            let all = [
                Permission::ClaimsRead,
                Permission::ClaimsWrite,
                LISTS_READ,
                Permission::ListsManage,
                ADD,
            ];
            let error = check_certificate(&cert(&scopes, &all))
                .unwrap_err()
                .to_string();
            assert!(error.contains(rule), "{scopes:?}: {error}");
        }
        let mut exts: Vec<String> = (1..=9).map(|i| format!("ext:e-{i}")).collect();
        exts.push("chat".into());
        assert!(check_scopes(&exts).is_err());
    }

    #[test]
    fn test_scopes_certificate_pay_without_contacts_refused() {
        // A paying app reads the deny list, so it never pays a counterparty the user
        // blocked (Paykit spec 3.1). `permissions` are exactly what the scopes need, so only
        // the rule refuses it.
        for scopes in [&["pay"][..], &["pay", "allowance"], &["pay", "request"]] {
            let permissions = permissions_for(&owned(scopes));
            let error = check_certificate(&cert(scopes, &permissions))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("pay requires contacts"),
                "{scopes:?}: {error}"
            );
        }
    }

    #[test]
    fn test_scopes_certificate_pay_with_contacts_accepted() {
        for scopes in [
            &["pay", "contacts"][..],
            &["pay", "allowance", "contacts"],
            &["pay", "request", "contacts", "manage_contacts"],
        ] {
            let permissions = permissions_for(&owned(scopes));
            assert!(permissions.contains(&LISTS_READ), "{scopes:?}");
            check_certificate(&cert(scopes, &permissions)).unwrap();
        }
    }

    #[test]
    fn test_scopes_bounds_count_only_paykit_scopes() {
        // Eight labels, eight extension scopes and other protocols' scopes beside them: within
        // Paykit's bounds, and the Switch's 32 in all.
        let mut scopes: Vec<String> = (1..=8).map(|i| format!("request:shop{i}")).collect();
        scopes.extend((1..=8).map(|i| format!("ext:e-{i}")));
        scopes.extend((1..=16).map(|i| format!("chat{i}")));
        assert_eq!(scopes.len(), 32);
        let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
        check_certificate(&cert(&names, &[LISTS_READ, ADD])).unwrap();
        // One more is over the Switch's total, whoever's it is.
        let mut over = names.clone();
        over.push("chat17");
        assert!(switch_scopes::check_scopes(&owned(&over)).is_err());
    }

    #[test]
    fn test_label_and_prefix_syntax() {
        assert!(Label::parse("paykit").is_ok());
        assert!(Label::parse("shop1").is_ok());
        assert!(Label::parse(&"a".repeat(32)).is_ok());
        assert!(Label::parse(&"a".repeat(33)).is_err());
        assert!(matches!(Label::new("A"), Err(Error::Validation(_))));
    }

    #[test]
    fn test_scopes_extension_prefix_holds_a_hyphen() {
        for good in [
            "paykit-cap",
            "a-b",
            "x-",
            "1-2",
            &format!("a-{}", "b".repeat(30)),
        ] {
            assert!(ExtensionPrefix::parse(good).is_ok(), "{good}");
        }
        // Without a `-` a prefix could be a protocol's name, Paykit's included.
        for bad in [
            "paykit",
            "cap",
            "chat",
            "x",
            "-x",
            "Paykit-cap",
            "a_b-c",
            "",
        ] {
            assert!(ExtensionPrefix::parse(bad).is_err(), "{bad}");
        }
        assert!(ExtensionPrefix::parse(&format!("a-{}", "b".repeat(31))).is_err());
        assert!(matches!(
            ExtensionPrefix::new("cap"),
            Err(Error::Validation(_))
        ));
    }
}
