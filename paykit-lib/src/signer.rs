//! Paykit as the signer knows it (Paykit spec 3.1, 3.2, 3.4, 3.5; Switch spec 1, 3.2).
//!
//! A signer issues an app's certificate only for scopes a protocol it registered owns, with
//! the switch permissions that protocol's table gives them, and keys for the scopes the
//! protocol says have them. [`PaykitProtocol`] is Paykit's answer to each of those, from the
//! same rules Paykit's verifiers apply ([`crate::scopes`]), so a certificate the signer issues
//! for Paykit's scopes is one Paykit's verifiers take. A host registers it once, in the
//! signer's `Protocols` set:
//!
//! ```
//! use std::sync::Arc;
//! use paykit_lib::signer::PaykitProtocol;
//! use pubky_switch_signer::Protocols;
//!
//! let protocols = Protocols::new(vec![Arc::new(PaykitProtocol)]).unwrap();
//! let terms = protocols.terms(&["pay".into(), "contacts".into()]).unwrap();
//! assert_eq!(
//!     terms.permission_names(),
//!     ["claims.read", "claims.write", "lists.read", "conversation.add"]
//! );
//! ```
//!
//! Security: these answers decide which permissions and keys a certificate carries. They are
//! pure and deterministic, as the signer requires: a signer that receives the identity in a
//! hand-over judges the same certificates the same way.

use pubky_switch_core::scopes::Permission;
use pubky_switch_signer::{GrantPath, Protocol, ScopeDef};

use crate::scopes::{
    check_requested_scopes, holds_paykit_scope, permissions_for, PAYKIT_PREFIX,
    PAYKIT_PROFILE_BLOBS_PATH, PAYKIT_PROFILE_PATH, PAYKIT_SCOPE_NAMES,
};

/// Paykit's protocol on the Switch, for the signer's `Protocols` set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaykitProtocol;

/// The one of Paykit's scope roots without keys (Paykit spec 3.5): `ext`, whose extension
/// scopes `ext:<prefix>` have none.
const UNKEYED_ROOT: &str = "ext";

/// What the host shows the user for the Paykit Profile's grant path.
const PROFILE_PURPOSE: &str = "Edit the Paykit Profile, the identity's public payment profile";

/// What the host shows the user for the grant path of the Paykit Profile's files.
const BLOBS_PURPOSE: &str = "Store the Paykit Profile's files, such as its image";

fn owned(scopes: &[&str]) -> Vec<String> {
    scopes.iter().map(|s| (*s).to_owned()).collect()
}

impl Protocol for PaykitProtocol {
    /// `paykit`, the root Paykit's reserved scope roots are listed with (Switch wire 3.1
    /// "Scope roots").
    fn name(&self) -> &str {
        PAYKIT_PREFIX
    }

    /// Paykit's seven scope roots (Paykit spec 3.1, 3.2). Every one but `ext` has keys, and so
    /// do its sub-scopes, the labels of `request` (Paykit spec 3.5). The signer then owns
    /// every sub-scope of a root for Paykit, also one Paykit gives no meaning, such as `pay:x`,
    /// which [`Protocol::check_scopes`] refuses.
    fn scopes(&self) -> Vec<ScopeDef> {
        PAYKIT_SCOPE_NAMES
            .iter()
            .map(|name| {
                if *name == UNKEYED_ROOT {
                    ScopeDef::unkeyed(name)
                } else {
                    ScopeDef::keyed(name)
                }
            })
            .collect()
    }

    /// Paykit's rules on an app request (Paykit spec 3.1): no malformed extension or labelled
    /// selling scope, at most 8 of each, every scope with the scopes it requires, and no name
    /// Paykit gives no meaning, such as `pay:x` or a plain `ext`. The Switch's scope rule,
    /// which refuses `request` beside a label, has already held for the whole request.
    ///
    /// # Errors
    ///
    /// The rule the request breaks; it never repeats a scope.
    fn check_scopes(&self, scopes: &[&str]) -> Result<(), String> {
        check_requested_scopes(&owned(scopes)).map_err(|error| error.to_string())
    }

    /// The switch permissions Paykit's table gives `scopes` (Paykit spec 3.2), the list
    /// Paykit's verifiers derive again.
    fn permissions(&self, scopes: &[&str]) -> Vec<Permission> {
        permissions_for(&owned(scopes))
    }

    /// The Paykit Profile, `/pub/paykit/profile.json`, and its files, `/pub/paykit/blobs/`,
    /// offered to every app holding a Paykit scope: the user allows them or not at approval,
    /// and only an app the user allows edits them (Paykit spec 3.4).
    fn grant_paths(&self, app_id: &[u8; 16], scopes: &[&str]) -> Vec<GrantPath> {
        let _ = app_id;
        if !holds_paykit_scope(&owned(scopes)) {
            return Vec::new();
        }
        vec![
            GrantPath {
                path: PAYKIT_PROFILE_PATH.to_owned(),
                purpose: PROFILE_PURPOSE.to_owned(),
            },
            GrantPath {
                path: PAYKIT_PROFILE_BLOBS_PATH.to_owned(),
                purpose: BLOBS_PURPOSE.to_owned(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pubky_switch_core::keys::{AppKeys, IdentityKey};
    use pubky_switch_core::records::{AppCert, AppCertBody};
    use pubky_switch_signer::{Protocols, Refusal, ScopeTerms, SignerError};

    use super::*;
    use crate::scopes::{check_certificate, keyed_scopes};

    /// Another protocol, beside Paykit, as a wallet that also chats registers it.
    struct Chat;

    impl Protocol for Chat {
        fn name(&self) -> &str {
            "chat"
        }
        fn scopes(&self) -> Vec<ScopeDef> {
            vec![ScopeDef::keyed("chat")]
        }
        fn check_scopes(&self, _: &[&str]) -> Result<(), String> {
            Ok(())
        }
        fn permissions(&self, _: &[&str]) -> Vec<Permission> {
            vec![Permission::ConversationAdd, Permission::ListsRead]
        }
    }

    fn paykit() -> Protocols {
        Protocols::new(vec![Arc::new(PaykitProtocol)]).unwrap()
    }

    fn names(scopes: &[&str]) -> Vec<String> {
        owned(scopes)
    }

    fn terms(protocols: &Protocols, scopes: &[&str]) -> ScopeTerms {
        protocols.terms(&names(scopes)).unwrap()
    }

    fn refusal(protocols: &Protocols, scopes: &[&str]) -> Refusal {
        match protocols.terms(&names(scopes)) {
            Err(SignerError::Refused(refusal)) => refusal,
            other => panic!("not refused: {other:?}"),
        }
    }

    /// A certificate carrying what the signer's terms give `scopes`, as Paykit's verifiers
    /// read it.
    fn certificate(scopes: &[&str], terms: &ScopeTerms) -> AppCert {
        let identity = IdentityKey::from_secret([6; 32]);
        let app = AppKeys::from_secrets([4; 32], [5; 32]);
        AppCert::issue(
            &identity,
            AppCertBody::new(
                identity.public(),
                app.signing_public(),
                app.bundle_public(),
                names(scopes),
                &terms.permissions,
                1_767_225_600_000,
                1_767_225_600_000 + 86_400_000,
            ),
        )
        .unwrap()
    }

    #[test]
    fn test_signer_protocol_registers_under_the_reserved_name() {
        let protocols = paykit();
        assert_eq!(protocols.names(), ["paykit"]);
        for scope in [
            "pay",
            "request",
            "request:shop1",
            "allowance",
            "allowance_execute",
            "contacts",
            "manage_contacts",
            "ext:paykit-cap",
        ] {
            assert!(protocols.knows_scope(scope), "{scope}");
        }
        assert!(!protocols.knows_scope("chat"));
    }

    #[test]
    fn test_signer_protocol_keys_follow_paykit_spec_3_5() {
        let protocols = paykit();
        for scope in [
            "pay",
            "request",
            "request:shop1",
            "allowance",
            "allowance_execute",
            "contacts",
            "manage_contacts",
        ] {
            assert!(protocols.has_keys(scope), "{scope}");
        }
        assert!(!protocols.has_keys("ext:paykit-cap"));
        let scopes = ["pay", "request", "contacts", "ext:paykit-cap"];
        assert_eq!(
            terms(&protocols, &scopes).keyed,
            keyed_scopes(&names(&scopes))
        );
    }

    #[test]
    fn test_signer_protocol_certificates_pass_paykits_verifiers() {
        let protocols = paykit();
        for scopes in [
            &["pay", "request", "contacts", "manage_contacts"][..],
            &["pay", "allowance", "allowance_execute", "contacts"],
            &["pay", "allowance", "contacts"],
            &["pay", "contacts"],
            &["request:shop1", "request:shop2"],
            &["contacts"],
            &["ext:paykit-cap", "request"],
        ] {
            let terms = terms(&protocols, scopes);
            assert_eq!(
                terms.permissions,
                permissions_for(&names(scopes)),
                "{scopes:?}"
            );
            check_certificate(&certificate(scopes, &terms)).unwrap();
        }
    }

    #[test]
    fn test_signer_protocol_beside_another_protocol_passes_paykits_verifiers() {
        let protocols = Protocols::new(vec![Arc::new(PaykitProtocol), Arc::new(Chat)]).unwrap();
        let scopes = ["contacts", "chat"];
        let terms = terms(&protocols, &scopes);
        assert_eq!(terms.permission_names(), ["lists.read", "conversation.add"]);
        assert_eq!(terms.keyed, names(&scopes));
        check_certificate(&certificate(&scopes, &terms)).unwrap();
    }

    #[test]
    fn test_signer_protocol_refuses_requests_paykits_rules_refuse() {
        let protocols = paykit();
        let nine = |root: &str| -> Vec<String> { (1..=9).map(|n| format!("{root}{n}")).collect() };
        let extensions = nine("ext:paykit-x");
        let labels = nine("request:shop");
        for scopes in [
            names(&["allowance_execute", "allowance"]),
            names(&["allowance_execute", "pay"]),
            names(&["allowance_execute", "allowance", "pay"]),
            names(&["pay"]),
            names(&["pay", "allowance"]),
            names(&["manage_contacts"]),
            names(&["ext:cap"]),
            names(&["ext:paykit"]),
            names(&["pay:x"]),
            names(&["pay", "contacts", "ext"]),
            extensions,
            labels,
        ] {
            match protocols.terms(&scopes) {
                Err(SignerError::Refused(Refusal::ProtocolRule { protocol, reason })) => {
                    assert_eq!(protocol, "paykit");
                    // The reason names the rule, never the request's own text.
                    assert!(
                        !reason.contains("shop") && !reason.contains("paykit-x"),
                        "{reason}"
                    );
                }
                other => panic!("{scopes:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn test_signer_protocol_request_beside_a_label_refused_by_the_scope_rule() {
        assert!(matches!(
            refusal(&paykit(), &["request", "request:shop1"]),
            Refusal::Scopes(_)
        ));
    }

    #[test]
    fn test_signer_protocol_another_protocols_scope_unknown_without_it() {
        assert!(matches!(
            refusal(&paykit(), &["pay", "contacts", "chat"]),
            Refusal::UnknownScopes { scopes } if scopes == ["chat"]
        ));
    }

    #[test]
    fn test_signer_protocol_offers_the_paykit_profile_and_its_files() {
        let protocols = Protocols::new(vec![Arc::new(PaykitProtocol), Arc::new(Chat)]).unwrap();
        let paths = protocols
            .grant_paths(&[3; 16], &names(&["request:shop1", "chat"]))
            .unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].path, "/pub/paykit/profile.json");
        assert_eq!(paths[1].path, "/pub/paykit/blobs/");
        assert!(protocols
            .grant_paths(&[3; 16], &names(&["chat"]))
            .unwrap()
            .is_empty());
        // A paying app's request holds the `contacts` that `pay` requires, and is offered
        // the Paykit Profile too.
        let wallet = names(&["pay", "contacts"]);
        protocols.terms(&wallet).unwrap();
        let paths = protocols.grant_paths(&[3; 16], &wallet).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].path, "/pub/paykit/profile.json");
    }

    #[test]
    fn test_signer_protocol_pay_request_without_contacts_refused() {
        let protocols = paykit();
        for scopes in [&["pay"][..], &["pay", "allowance"], &["pay", "request"]] {
            match refusal(&protocols, scopes) {
                Refusal::ProtocolRule { protocol, reason } => {
                    assert_eq!(protocol, "paykit");
                    assert!(reason.contains("pay requires contacts"), "{reason}");
                }
                other => panic!("{scopes:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn test_signer_protocol_pay_request_with_contacts_reads_the_lists() {
        let protocols = paykit();
        for scopes in [&["pay", "contacts"][..], &["pay", "allowance", "contacts"]] {
            let terms = terms(&protocols, scopes);
            assert!(
                terms.permissions.contains(&Permission::ListsRead),
                "{scopes:?}"
            );
            check_certificate(&certificate(scopes, &terms)).unwrap();
        }
    }

    /// An Allowance executor's scopes (Paykit spec 3.1).
    const EXECUTOR: [&str; 4] = ["pay", "allowance", "allowance_execute", "contacts"];

    #[test]
    fn test_signer_protocol_executor_renewal_keeps_its_terms() {
        // An automatic renewal keeps the certificate's scopes, and the signer renews it only
        // while the terms those scopes get now are the ones it holds (Switch spec 3.2):
        // an executor's are, so it is renewed with `allowance_execute` and every scope it
        // requires.
        let protocols = paykit();
        let held = certificate(&EXECUTOR, &terms(&protocols, &EXECUTOR));
        let now = protocols.terms(held.scopes()).unwrap();
        assert_eq!(now.permissions, held.permissions());
        assert_eq!(held.scopes(), names(&EXECUTOR).as_slice());
        let scopes: Vec<&str> = held.scopes().iter().map(String::as_str).collect();
        check_certificate(&certificate(&scopes, &now)).unwrap();
    }

    #[test]
    fn test_signer_protocol_executor_with_other_terms_isnt_renewed_as_it_is() {
        // An executor's certificate holding other permissions than Paykit's table gives its
        // scopes now: the signer leaves its renewal to the user (Switch spec 3.2).
        let protocols = paykit();
        let mut fewer = terms(&protocols, &EXECUTOR);
        fewer.permissions.retain(|p| *p != Permission::ClaimsWrite);
        let held = certificate(&EXECUTOR, &fewer);
        let now = protocols.terms(held.scopes()).unwrap();
        assert_ne!(now.permissions, held.permissions());
    }

    #[test]
    fn test_signer_protocol_dropping_allowance_execute_rotates_the_epoch() {
        // A reduction that removes a scope with keys rotates the data epoch (Paykit spec 3.2,
        // 3.5): `allowance_execute` has keys, and the scopes left still pass Paykit's rules.
        let protocols = paykit();
        assert!(protocols.has_keys("allowance_execute"));
        let reduced = ["pay", "allowance", "contacts"];
        check_certificate(&certificate(&reduced, &terms(&protocols, &reduced))).unwrap();
    }

    #[test]
    fn test_signer_protocol_executor_without_a_scope_it_requires_refused() {
        // A reduction that drops a scope `allowance_execute` requires, but keeps it, is
        // refused (Paykit spec 3.1).
        let protocols = paykit();
        for scopes in [
            &["allowance", "allowance_execute", "contacts"][..],
            &["pay", "allowance_execute", "contacts"],
        ] {
            assert!(
                matches!(refusal(&protocols, scopes), Refusal::ProtocolRule { .. }),
                "{scopes:?}"
            );
        }
    }

    #[test]
    fn test_signer_protocol_grant_paths_cover_neither_identity_json_nor_the_backup() {
        // The signer refuses a capability covering the Switch's `identity.json` or its
        // backup, which only it writes (Switch spec 3.4, 12.8): no grant path Paykit offers
        // is one, or a folder holding one.
        use pubky_switch_core::records::{IDENTITY_PATH, SIGNER_BACKUP_PATH};
        let covers = |path: &str, file: &str| {
            path == file || (path.ends_with('/') && file.starts_with(path))
        };
        let paths = PaykitProtocol.grant_paths(&[7; 16], &EXECUTOR);
        assert!(!paths.is_empty());
        for granted in &paths {
            for file in [IDENTITY_PATH, SIGNER_BACKUP_PATH] {
                assert!(
                    !covers(&granted.path, file),
                    "{} covers {file}",
                    granted.path
                );
            }
        }
        assert!(covers("/pub/", IDENTITY_PATH), "a folder holding it would");
    }
}
