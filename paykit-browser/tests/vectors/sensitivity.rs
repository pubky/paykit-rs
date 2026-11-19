//! Values of the set that change when one of their inputs changes, which paykit-lib's checks
//! of the files do not assert: each test reads the value from its built-in file, rebuilds it
//! from its inputs, and shows that another input gives another value.

use paykit_lib::collections::Collection;
use paykit_lib::derive::terms_hash;
use paykit_lib::grant::{EXECUTOR_GRANT_LABEL, LEDGER_HANDOFF_LABEL};
use paykit_lib::scopes::Label;
use pubky_switch_core::enc::b64;
use pubky_switch_core::sig::verify;

use crate::common::{arr, b64n, hex, message_json, st, unhex, Keys, SHOP1};
use crate::{built_in_file, built_in_set, refuse_while_writing};

/// The value `read` returns, with the built-in set as paykit-lib's corpus, so that the helpers
/// of `common` that read a file read the built-in one.
fn in_built_in_set<T>(read: impl FnOnce() -> T) -> T {
    let mut value = None;
    crate::common::in_corpus(built_in_set().clone(), || value = Some(read()));
    value.expect("the read ran")
}

conformance_test! {
    /// `keys.json` scope keys: each key rebuilt from its identity, data epoch and scope, and
    /// the next data epoch gives another key (Switch wire 1.5).
    fn test_scope_key_changes_with_data_epoch() {
        refuse_while_writing();
        let keys = Keys::from_seeds();
        let cases = arr(built_in_file("keys.json"), "scope_keys");
        assert!(!cases.is_empty(), "keys.json: no scope key");
        for case in cases {
            let identity = keys.identity_named(st(case, "identity"));
            let scope = st(case, "scope");
            let epoch = case["data_epoch"]
                .as_u64()
                .unwrap_or_else(|| panic!("{scope}: data_epoch"));
            let key = st(case, "key_hex");
            assert_eq!(hex(&identity.scope_key(epoch, scope)), key, "{scope}");
            assert_ne!(hex(&identity.scope_key(epoch + 1, scope)), key, "{scope}");
        }
    }
}

conformance_test! {
    /// `identifiers.json` terms hash: rebuilt from its proposal and acceptance in
    /// `messages.json`, and the two messages in the other order give another hash (Paykit
    /// wire 1.5).
    fn test_terms_hash_changes_with_message_order() {
        refuse_while_writing();
        let t = &built_in_file("identifiers.json")["terms_hash"];
        let (proposal, acceptance) = in_built_in_set(|| {
            (
                message_json(st(t, "proposal")),
                message_json(st(t, "acceptance")),
            )
        });
        let held = st(t, "terms_hash");
        assert_eq!(b64(&terms_hash(&proposal, &acceptance)), held);
        assert_ne!(b64(&terms_hash(&acceptance, &proposal)), held);
    }
}

conformance_test! {
    /// `identifiers.json` policies: `payee_activity`'s policy under the label `shop1` and its
    /// fixed policy each hash to their file's hash, and the two hashes differ (Paykit wire 1.5,
    /// Switch wire 1.5).
    fn test_policy_hash_changes_with_label() {
        refuse_while_writing();
        let cases = arr(&built_in_file("identifiers.json")["policies"], "cases");
        let fixed: Vec<&str> = cases
            .iter()
            .filter(|case| st(case, "name") == Collection::PayeeActivity.name())
            .map(|case| st(case, "policy_hash"))
            .collect();
        // The labelled policy is the one case that names no collection.
        let labelled: Vec<&str> = cases
            .iter()
            .filter(|case| Collection::from_name(st(case, "name")).is_none())
            .map(|case| st(case, "policy_hash"))
            .collect();
        assert_eq!(fixed.len(), 1, "payee_activity's fixed policy");
        assert_eq!(labelled.len(), 1, "the labelled policy");
        let shop1 = Label::parse(SHOP1).expect("the label shop1");
        let hash_of = |label: Option<&Label>| {
            b64(&Collection::PayeeActivity
                .policy(label)
                .hash()
                .expect("a policy hash"))
        };
        assert_eq!(hash_of(None), fixed[0]);
        assert_eq!(hash_of(Some(&shop1)), labelled[0]);
        assert_ne!(fixed[0], labelled[0]);
    }
}

conformance_test! {
    /// `signatures.json` executor grant: the executor's signature verifies under the executor
    /// grant's label over its fields, and not under the ledger handoff's (Paykit wire 5.4,
    /// 5.10).
    fn test_executor_grant_signature_refused_under_handoff_label() {
        refuse_while_writing();
        let cases = arr(built_in_file("signatures.json"), "signatures");
        let case = cases
            .iter()
            .find(|case| st(case, "label") == EXECUTOR_GRANT_LABEL)
            .expect("the executor grant's case");
        let fields: Vec<Vec<u8>> = arr(case, "fields")
            .iter()
            .map(|field| unhex(st(field, "hex")))
            .collect();
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let public = b64n::<32>(st(case, "public_key"));
        assert_eq!(
            public,
            Keys::from_seeds().app("executor").keys.signing_public()
        );
        let signature = b64n::<64>(st(case, "signature"));
        assert!(verify(&public, EXECUTOR_GRANT_LABEL, &refs, &signature).is_ok());
        assert!(verify(&public, LEDGER_HANDOFF_LABEL, &refs, &signature).is_err());
    }
}
