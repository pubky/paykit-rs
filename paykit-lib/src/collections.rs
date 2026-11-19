//! Paykit's collections and their fixed policies (Paykit spec 6, Paykit wire 5.1).
//!
//! Each collection's records use one fixed policy; a `payee_activity` record of a request
//! sent under a label uses that label's policy instead. Apps ignore any record in these
//! collections whose policy differs, so no app can plant a record readable under another
//! scope or another label. Record keys derive from the collection, the blinded natural key
//! and the policy hash (Switch wire 1.5), so a labelled request's record sits at another key
//! than an unlabelled one's.

use pubky_switch_core::ident::{NamingKey, Policy};

use crate::error::Result;
use crate::scopes::{Label, PaykitScope, ScopeName};

/// One of Paykit's collections (Paykit spec 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Collection {
    /// `payer_activity`: one record per request received, Billing Period, or payment made
    /// without a request.
    PayerActivity,
    /// `payee_activity`: the same, from the payee's side.
    PayeeActivity,
    /// `payment_lists`: one record per payee and publishing app.
    PaymentLists,
    /// `allowance_proposals`: one ordinary record per pending proposal.
    AllowanceProposals,
    /// `allowances`: one record per Allowance, on either side.
    Allowances,
    /// `allowance_ledgers`: one ledger per Allowance.
    AllowanceLedgers,
    /// `block_floor`: one record, the identity's block floor (Paykit wire 5.6).
    BlockFloor,
    /// `settlements`: at most 1,024 bucket records, the marks of the settlements any own
    /// payee app has credited (Paykit wire 5.7).
    Settlements,
}

impl Collection {
    /// Every collection.
    pub const ALL: [Collection; 8] = [
        Self::PayerActivity,
        Self::PayeeActivity,
        Self::PaymentLists,
        Self::AllowanceProposals,
        Self::Allowances,
        Self::AllowanceLedgers,
        Self::BlockFloor,
        Self::Settlements,
    ];

    /// The collection's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::PayerActivity => "payer_activity",
            Self::PayeeActivity => "payee_activity",
            Self::PaymentLists => "payment_lists",
            Self::AllowanceProposals => "allowance_proposals",
            Self::Allowances => "allowances",
            Self::AllowanceLedgers => "allowance_ledgers",
            Self::BlockFloor => "block_floor",
            Self::Settlements => "settlements",
        }
    }

    /// The collection of a name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The collection's fixed policy (Paykit wire 5.1).
    pub fn fixed_policy(self) -> Policy {
        match self {
            Self::PayerActivity | Self::PaymentLists | Self::BlockFloor => {
                Policy::new("paykit_pay", &["paykit_pay"])
            }
            Self::PayeeActivity => Policy::new("paykit_request", &["paykit_request"]),
            Self::Allowances => Policy::new("paykit_allowance", &["paykit_allowance"]),
            Self::AllowanceProposals => Policy::new("paykit_allowance", &["paykit_allowance"]),
            Self::AllowanceLedgers => {
                Policy::new("paykit_allowance", &["paykit_allowance_execute"])
            }
            Self::Settlements => {
                Policy::new("paykit_settlements", &["paykit_settlements"]).permanent(None)
            }
        }
    }

    /// The policy of a record about a request sent under `label` (Paykit spec 6): a
    /// `payee_activity` record of a labelled request has its label's policy, `read_scope =
    /// paykit_request:<label>` and `write_scopes =
    /// ["paykit_request:<label>"]`; every other record keeps its collection's fixed policy, a
    /// `payer_activity` record whatever the request's label, and a `settlements` bucket one
    /// policy for every label.
    pub fn policy(self, label: Option<&Label>) -> Policy {
        match (self, label) {
            (Self::PayeeActivity, Some(label)) => {
                let scope = label.scope();
                Policy::new(&scope, &[scope.as_str()])
            }
            _ => self.fixed_policy(),
        }
    }

    /// Which policy a record's policy is, if it is one this collection allows (Paykit wire
    /// 5.1): `Some(None)` for the fixed policy, `Some(Some(label))` for a labelled policy of
    /// `payee_activity`, and `None` for any other, which apps ignore.
    pub fn policy_label(self, policy: &Policy) -> Option<Option<Label>> {
        if *policy == self.fixed_policy() {
            return Some(None);
        }
        if self != Self::PayeeActivity || policy.write_scopes != [policy.read_scope.clone()] {
            return None;
        }
        match PaykitScope::classify(&policy.read_scope) {
            ScopeName::Paykit(PaykitScope::RequestLabel(label)) => Some(Some(label)),
            _ => None,
        }
    }

    /// The record key of a natural key in this collection, under the policy for `label`
    /// ([`Collection::policy`]), blinded with the naming key of the identity whose switch
    /// holds it (Switch wire 1.5).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Switch`] if the policy hash can't be computed.
    pub fn record_key(
        self,
        naming_key: &NamingKey,
        natural_key: &[u8],
        label: Option<&Label>,
    ) -> Result<[u8; 32]> {
        let policy = self.policy(label);
        Ok(naming_key.record_key(self.name(), natural_key, &policy.hash()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collections_fixed_policies() {
        assert_eq!(
            Collection::PayerActivity.fixed_policy(),
            Policy::new("paykit_pay", &["paykit_pay"])
        );
        assert_eq!(
            Collection::AllowanceLedgers.fixed_policy(),
            Policy::new("paykit_allowance", &["paykit_allowance_execute"])
        );
        assert_eq!(
            Collection::BlockFloor.fixed_policy(),
            Policy::new("paykit_pay", &["paykit_pay"])
        );
        assert_eq!(
            Collection::Settlements.fixed_policy(),
            Policy::new("paykit_settlements", &["paykit_settlements"]).permanent(None)
        );
        for c in Collection::ALL {
            assert_eq!(Collection::from_name(c.name()), Some(c));
        }
        assert_eq!(Collection::from_name("other"), None);
    }

    #[test]
    fn test_collections_labelled_policy_only_for_payee_activity() {
        let label = Label::parse("shop1").unwrap();
        let labelled = Collection::PayeeActivity.policy(Some(&label));
        assert_eq!(
            labelled,
            Policy::new("paykit_request:shop1", &["paykit_request:shop1"])
        );
        assert_eq!(
            Collection::PayeeActivity.policy_label(&labelled),
            Some(Some(label.clone()))
        );
        assert_eq!(
            Collection::PayerActivity.policy(Some(&label)),
            Collection::PayerActivity.fixed_policy()
        );
        assert_eq!(Collection::PayerActivity.policy_label(&labelled), None);
    }

    #[test]
    fn test_collections_refuse_other_policies() {
        for policy in [
            Policy::new("paykit_request:shop1", &["paykit_request"]),
            Policy::new("paykit_request:Shop1", &["paykit_request:Shop1"]),
            Policy::new("paykit_pay", &["paykit_pay", "paykit_request"]),
            Policy::new("paykit_request", &["paykit_pay"]),
        ] {
            assert_eq!(
                Collection::PayeeActivity.policy_label(&policy),
                None,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn test_collections_settlements_have_one_policy_for_every_label() {
        let label = Label::parse("shop1").unwrap();
        let fixed = Collection::Settlements.fixed_policy();
        assert_eq!(Collection::Settlements.policy(Some(&label)), fixed);
        assert_eq!(Collection::Settlements.policy_label(&fixed), Some(None));
        for other in [
            Policy::new("paykit_settlements", &["paykit_settlements"]),
            Policy::new("paykit_settlements", &["paykit_settlements"]).permanent(Some(1)),
            Policy::new("paykit_request", &["paykit_request"]),
            Policy::new("paykit_request:shop1", &["paykit_request:shop1"]),
            Policy::new(
                "paykit_settlements",
                &["paykit_settlements", "paykit_request"],
            ),
            Policy::new("paykit_settlements", &["paykit_request"]),
        ] {
            assert_eq!(
                Collection::Settlements.policy_label(&other),
                None,
                "{other:?}"
            );
        }
        let nk = NamingKey::from_bytes([1; 32]);
        assert_eq!(
            Collection::Settlements
                .record_key(&nk, b"k", Some(&label))
                .unwrap(),
            Collection::Settlements.record_key(&nk, b"k", None).unwrap()
        );
    }

    #[test]
    fn test_collections_record_key_depends_on_the_label() {
        let nk = NamingKey::from_bytes([1; 32]);
        let label = Label::parse("shop1").unwrap();
        let fixed = Collection::PayeeActivity
            .record_key(&nk, b"k", None)
            .unwrap();
        let labelled = Collection::PayeeActivity
            .record_key(&nk, b"k", Some(&label))
            .unwrap();
        assert_ne!(fixed, labelled);
        let payer = Collection::PayerActivity
            .record_key(&nk, b"k", Some(&label))
            .unwrap();
        assert_eq!(
            payer,
            Collection::PayerActivity
                .record_key(&nk, b"k", None)
                .unwrap()
        );
    }
}
