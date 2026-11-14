//! Paykit's message kinds and the scopes that send and receive them (Paykit spec 3.3, 3.4,
//! 5; Paykit wire 4.1, 4.3).
//!
//! The Switch runs the common recipient checks of a message (Switch wire 5.1); Paykit gives
//! it the rules of its kinds through [`PaykitKindRules`]: the opaque members of its bodies
//! (Paykit wire 1.1), the one kind sent between own apps (check 6) and the scopes of each
//! kind, narrowed by label (check 8). The body, check 10, is [`crate::bodies`].
//!
//! Security: check 8 decides which app may send a kind and which may take it. A labelled selling
//! scope counts as `paykit_request` for the kind, and the request's `label` then narrows it, so a
//! hosted app never sends a request outside its labels or takes the payer's replies about the
//! identity's own requests.

use pubky_switch_core::json::OpaqueMember;
use pubky_switch_core::message::Message;
use pubky_switch_core::receive::KindRules;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::scopes as switch_scopes;

use crate::error::{invalid, Error, Result};
use crate::scopes::{self, ExtensionPrefix, Label, PAYKIT_PREFIX};

/// The most bytes of JSON a kind that activity records store may take (Paykit spec 5,
/// Paykit wire 4.1).
pub const MAX_RECORDED_KIND_LEN: usize = 16_384;

/// The opaque members of Paykit's message bodies (Paykit wire 1.1): a body's `metadata`, of
/// every kind, extension kinds included, and the `proof` of a `paykit.payment_proof`. Every
/// other member is checked by the Switch's integer rule.
pub const OPAQUE_MEMBERS: &[OpaqueMember<'static>] = &[
    OpaqueMember {
        kind: None,
        member: "metadata",
    },
    OpaqueMember {
        kind: Some("paykit.payment_proof"),
        member: "proof",
    },
];

/// Who may send or receive a kind (Paykit spec 3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// An app holding `paykit_pay`.
    Pay,
    /// An app holding `paykit_request`, or a labelled selling scope, which counts as
    /// `paykit_request` for the kind before labels narrow it.
    Request,
    /// An app holding `paykit_allowance`.
    Allowance,
    /// An app holding `paykit_allowance_execute`.
    AllowanceExecute,
    /// Any Paykit App: one holding at least one of Paykit's scopes.
    Any,
}

/// One of Paykit's own kinds, `paykit.<name>` (Paykit spec 5, Paykit wire 4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `paykit.payment_request` (Paykit wire 4.5).
    PaymentRequest,
    /// `paykit.payment_request_acceptance` (Paykit wire 4.6).
    PaymentRequestAcceptance,
    /// `paykit.payment_request_rejection` (Paykit wire 4.7).
    PaymentRequestRejection,
    /// `paykit.payment_request_cancellation` (Paykit wire 4.8).
    PaymentRequestCancellation,
    /// `paykit.payment_period_details` (Paykit wire 4.9).
    PaymentPeriodDetails,
    /// `paykit.payment_proof` (Paykit wire 4.10).
    PaymentProof,
    /// `paykit.payment_receipt` (Paykit wire 4.11).
    PaymentReceipt,
    /// `paykit.resend_request` (Paykit wire 4.12).
    ResendRequest,
    /// `paykit.private_payment_list` (Paykit wire 4.13).
    PrivatePaymentList,
    /// `paykit.private_payment_list_request` (Paykit wire 4.14).
    PrivatePaymentListRequest,
    /// `paykit.allowance_proposal` (Paykit wire 4.15).
    AllowanceProposal,
    /// `paykit.allowance_acceptance` (Paykit wire 4.16).
    AllowanceAcceptance,
    /// `paykit.allowance_rejection` (Paykit wire 4.16).
    AllowanceRejection,
    /// `paykit.allowance_end` (Paykit wire 4.16).
    AllowanceEnd,
    /// `paykit.executor_notice` (Paykit wire 4.17), between apps of one identity only.
    ExecutorNotice,
}

impl Kind {
    /// Every Paykit kind.
    pub const ALL: [Kind; 15] = [
        Self::PaymentRequest,
        Self::PaymentRequestAcceptance,
        Self::PaymentRequestRejection,
        Self::PaymentRequestCancellation,
        Self::PaymentPeriodDetails,
        Self::PaymentProof,
        Self::PaymentReceipt,
        Self::ResendRequest,
        Self::PrivatePaymentList,
        Self::PrivatePaymentListRequest,
        Self::AllowanceProposal,
        Self::AllowanceAcceptance,
        Self::AllowanceRejection,
        Self::AllowanceEnd,
        Self::ExecutorNotice,
    ];

    /// The kind string, `paykit.<name>`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PaymentRequest => "paykit.payment_request",
            Self::PaymentRequestAcceptance => "paykit.payment_request_acceptance",
            Self::PaymentRequestRejection => "paykit.payment_request_rejection",
            Self::PaymentRequestCancellation => "paykit.payment_request_cancellation",
            Self::PaymentPeriodDetails => "paykit.payment_period_details",
            Self::PaymentProof => "paykit.payment_proof",
            Self::PaymentReceipt => "paykit.payment_receipt",
            Self::ResendRequest => "paykit.resend_request",
            Self::PrivatePaymentList => "paykit.private_payment_list",
            Self::PrivatePaymentListRequest => "paykit.private_payment_list_request",
            Self::AllowanceProposal => "paykit.allowance_proposal",
            Self::AllowanceAcceptance => "paykit.allowance_acceptance",
            Self::AllowanceRejection => "paykit.allowance_rejection",
            Self::AllowanceEnd => "paykit.allowance_end",
            Self::ExecutorNotice => "paykit.executor_notice",
        }
    }

    /// The kind of a kind string; `None` for any other.
    pub fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == kind)
    }

    /// The scopes, any one of which sends the kind (Paykit spec 3.3).
    pub fn senders(self) -> &'static [Role] {
        match self {
            Self::PaymentRequest
            | Self::PaymentPeriodDetails
            | Self::PaymentReceipt
            | Self::PrivatePaymentList => &[Role::Request],
            Self::PaymentRequestAcceptance
            | Self::PaymentRequestRejection
            | Self::PaymentProof
            | Self::PrivatePaymentListRequest => &[Role::Pay],
            Self::PaymentRequestCancellation => &[Role::Request, Role::Pay],
            Self::AllowanceProposal
            | Self::AllowanceAcceptance
            | Self::AllowanceRejection
            | Self::AllowanceEnd
            | Self::ExecutorNotice => &[Role::Allowance],
            Self::ResendRequest => &[Role::Any],
        }
    }

    /// The scopes, any one of which receives the kind (Paykit spec 3.3). A cancellation goes
    /// to the other side; a `resend_request` to any app, whose answer is filtered instead.
    pub fn recipients(self) -> &'static [Role] {
        match self {
            Self::PaymentRequest
            | Self::PaymentPeriodDetails
            | Self::PaymentReceipt
            | Self::PrivatePaymentList => &[Role::Pay],
            Self::PaymentRequestAcceptance
            | Self::PaymentRequestRejection
            | Self::PaymentProof
            | Self::PrivatePaymentListRequest => &[Role::Request],
            Self::PaymentRequestCancellation => &[Role::Request, Role::Pay],
            Self::AllowanceProposal
            | Self::AllowanceAcceptance
            | Self::AllowanceRejection
            | Self::AllowanceEnd => &[Role::Allowance],
            Self::ExecutorNotice => &[Role::AllowanceExecute],
            Self::ResendRequest => &[Role::Any],
        }
    }

    /// The `refs` counts the kind allows (Paykit wire 4.3).
    pub fn refs_counts(self) -> &'static [usize] {
        match self {
            Self::PaymentRequest
            | Self::ResendRequest
            | Self::PrivatePaymentList
            | Self::PrivatePaymentListRequest
            | Self::AllowanceProposal
            | Self::ExecutorNotice => &[0],
            Self::PaymentRequestAcceptance
            | Self::PaymentRequestRejection
            | Self::PaymentRequestCancellation
            | Self::PaymentPeriodDetails
            | Self::AllowanceAcceptance
            | Self::AllowanceRejection => &[1],
            Self::PaymentProof => &[0, 1, 2],
            Self::PaymentReceipt | Self::AllowanceEnd => &[1, 2],
        }
    }

    /// Whether activity records store the kind, so its JSON is at most 16 KiB (Paykit spec 5,
    /// Paykit wire 4.3 "Recorded").
    pub fn is_recorded_in_activity(self) -> bool {
        matches!(
            self,
            Self::PaymentRequest
                | Self::PaymentRequestAcceptance
                | Self::PaymentRequestRejection
                | Self::PaymentRequestCancellation
                | Self::PaymentPeriodDetails
                | Self::PaymentProof
                | Self::PaymentReceipt
        )
    }

    /// Whether the kind is sent between apps of one identity (Paykit wire 4.3): only
    /// `executor_notice`; every other kind goes between identities.
    pub fn sent_between_own_apps(self) -> bool {
        self == Self::ExecutorNotice
    }

    /// Whether the kind is one of the payer's replies, which copy a request's `label` and
    /// whose senders and recipients the label narrows (Paykit spec 3.4, Paykit wire 4.3).
    pub fn is_payer_reply(self) -> bool {
        matches!(
            self,
            Self::PaymentRequestAcceptance
                | Self::PaymentRequestRejection
                | Self::PaymentRequestCancellation
                | Self::PaymentProof
        )
    }
}

/// The longest name of an extension kind, in characters (Paykit wire 4.3).
pub const MAX_EXTENSION_NAME_LEN: usize = 64;

/// An extension kind `<prefix>.<name>` (Paykit spec 3.3, Paykit wire 4.3): a prefix of an
/// extension scope, which holds a `-`, then a name of 1 to 64 characters of `a-z`, `0-9` and
/// `_`. It carries no Paykit payment semantics. A kind whose prefix holds no `-`, such as
/// `chat.message`, is another protocol's or Paykit's own, never an extension kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExtensionKind {
    prefix: ExtensionPrefix,
    name: String,
}

impl ExtensionKind {
    /// An extension kind the caller supplies (Paykit wire 4.3).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for anything else, a Paykit kind `paykit.<name>` included.
    pub fn new(kind: &str) -> Result<Self> {
        Self::parse(kind).map_err(Error::into_validation)
    }

    /// Read an extension kind (Paykit wire 4.3).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for anything else, a Paykit kind `paykit.<name>` included.
    pub fn parse(kind: &str) -> Result<Self> {
        let refuse = || {
            Error::Invalid(
                "Paykit wire 4.3: an extension kind is <prefix>.<name>, the name 1 to 64 of a-z, 0-9 and _"
                    .into(),
            )
        };
        let (prefix, name) = kind.split_once('.').ok_or_else(refuse)?;
        let prefix = ExtensionPrefix::parse(prefix).map_err(|_| refuse())?;
        let name_ok = (1..=MAX_EXTENSION_NAME_LEN).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !name_ok {
            return Err(refuse());
        }
        Ok(Self {
            prefix,
            name: name.to_owned(),
        })
    }

    /// The prefix.
    pub fn prefix(&self) -> &ExtensionPrefix {
        &self.prefix
    }

    /// The name after the prefix.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The kind string.
    pub fn as_string(&self) -> String {
        format!("{}.{}", self.prefix.as_str(), self.name)
    }
}

/// A message kind as Paykit reads it: one of its own, or an extension kind (Paykit wire 4.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// A Paykit kind.
    Paykit(Kind),
    /// An extension kind.
    Extension(ExtensionKind),
}

impl MessageKind {
    /// Read a kind string (Paykit wire 4.1): a Paykit kind or an extension kind.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a kind Paykit doesn't know, such as `paykit.other`, which
    /// recipients drop.
    pub fn parse(kind: &str) -> Result<Self> {
        if let Some(k) = Kind::from_kind(kind) {
            return Ok(Self::Paykit(k));
        }
        if kind
            .split_once('.')
            .is_some_and(|(p, _)| p == PAYKIT_PREFIX)
        {
            return invalid("Paykit wire 4.1: a paykit kind recipients don't know");
        }
        ExtensionKind::parse(kind).map(Self::Extension)
    }
}

/// Whether `scopes` hold one of `roles`, a labelled selling scope counting as `paykit_request`
/// (Paykit wire 4.1 check 8; the labels then narrow it). [`Role::Any`] is any Paykit App:
/// a certificate holding at least one of Paykit's scopes, so a certificate of another
/// protocol only sends and receives no Paykit kind (Switch wire 3.1 "Several protocols").
fn holds_role(scopes: &[String], roles: &[Role]) -> bool {
    let has = |s: &str| scopes.iter().any(|x| x == s);
    roles.iter().any(|role| match role {
        Role::Any => scopes::holds_paykit_scope(scopes),
        Role::Pay => has("paykit_pay"),
        Role::Request => scopes::acts_as_payee(scopes),
        Role::Allowance => has("paykit_allowance"),
        Role::AllowanceExecute => has("paykit_allowance_execute"),
    })
}

/// The `label` member of a body, if it is a string; `None` when absent or `null`.
///
/// # Errors
///
/// [`Error::Invalid`] for a `label` that isn't a string.
fn body_label(message: &Message) -> Result<Option<&str>> {
    match message.body.get("label") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(label)) => Ok(Some(label)),
        Some(_) => invalid("Paykit wire 4.1 check 10: label isn't a string"),
    }
}

/// The sender's side of check 8 for a Paykit message (Paykit spec 3.3, 3.4; Paykit wire 4.1,
/// 4.5): what the sender's certificate alone decides, the same for a message received and
/// one read from shared state.
///
/// - the sender's scopes hold one that sends the kind;
/// - a `payment_request` with `label` needs `paykit_request` or `paykit_request:<label>` in the
///   sender's certificate, and one without needs `paykit_request`;
/// - a cancellation with `label` is the payer's, so it needs `paykit_pay` in the sender's
///   certificate;
/// - an extension kind `<prefix>.<name>` needs `paykit_ext:<prefix>` in the sender's certificate;
/// - a `resend_request` names no kinds: any Paykit App may send it, one holding at least one
///   of Paykit's scopes.
///
/// Security: a stored message is checked here when it is read (Paykit wire 5.2), as at the
/// inbox, so no own app can put in a shared record a message its sender wasn't allowed to
/// send, such as a request under a label its certificate doesn't hold, for another own app
/// to act on.
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule: the entry is dropped, or the record is invalid.
pub fn check_sender_scopes(message: &Message, sender: &[String]) -> Result<()> {
    let kind = match MessageKind::parse(&message.kind)? {
        MessageKind::Extension(ext) => {
            if !sender.contains(&ext.prefix().scope()) {
                return invalid(
                    "Paykit wire 4.1 check 8: an extension kind needs paykit_ext:<prefix> in both certificates",
                );
            }
            return Ok(());
        }
        MessageKind::Paykit(kind) => kind,
    };
    if !holds_role(sender, kind.senders()) {
        return invalid(
            "Paykit spec 3.3: the sender's certificate holds no scope that sends the kind",
        );
    }
    let label = body_label(message)?;
    if kind == Kind::PaymentRequest {
        let needs = label.map_or_else(
            || "paykit_request".to_owned(),
            |l| format!("{}:{l}", scopes::REQUEST),
        );
        if !switch_scopes::holds(sender, &needs) {
            return invalid(
                "Paykit wire 4.5: the sender's certificate doesn't hold the request's label",
            );
        }
    }
    if kind == Kind::PaymentRequestCancellation
        && label.is_some()
        && !sender.iter().any(|s| s == "paykit_pay")
    {
        return invalid(
            "Paykit spec 3.3: a labelled cancellation is the payer's, from an app holding paykit_pay",
        );
    }
    Ok(())
}

/// Check 8 for a Paykit message (Paykit spec 3.3, 3.4; Paykit wire 4.1, Switch wire 5.1):
/// the sender's side ([`check_sender_scopes`]), then the own scopes hold one that receives
/// the kind, and the labels and roles:
///
/// - a payer's acceptance, rejection, cancellation or proof with `label` needs `paykit_request` or
///   `paykit_request:<label>` in the own certificate, and one without needs `paykit_request`;
/// - a cancellation goes to the other role than its sender's: the payer's, from `paykit_pay`, at
///   an app the label rule above lets take it, or the payee's, which carries no label, from
///   `paykit_request` or a labelled selling scope at an app holding `paykit_pay`;
/// - an extension kind `<prefix>.<name>` needs `paykit_ext:<prefix>` in the own certificate too.
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule: the entry is dropped.
pub fn check_kind_scopes(message: &Message, sender: &[String], own: &[String]) -> Result<()> {
    check_sender_scopes(message, sender)?;
    let kind = match MessageKind::parse(&message.kind)? {
        MessageKind::Extension(ext) => {
            if !own.contains(&ext.prefix().scope()) {
                return invalid(
                    "Paykit wire 4.1 check 8: an extension kind needs paykit_ext:<prefix> in both certificates",
                );
            }
            return Ok(());
        }
        MessageKind::Paykit(kind) => kind,
    };
    if !holds_role(own, kind.recipients()) {
        return invalid(
            "Paykit spec 3.3: the own certificate holds no scope that receives the kind",
        );
    }
    let label = body_label(message)?;
    if kind == Kind::PaymentRequestCancellation {
        return check_cancellation_roles(label, sender, own);
    }
    if kind.is_payer_reply() && !reply_label_allowed(label, own) {
        return invalid(
            "Paykit spec 3.4: the reply's label is outside the recipient's certificate",
        );
    }
    Ok(())
}

/// A cancellation's roles (Paykit spec 3.3, Paykit wire 4.3, 4.8): its recipient holds the other
/// role than its sender. It is the payer's, sent from `paykit_pay` to an app that takes the payer's
/// replies under its label (Paykit spec 3.4), or the payee's, which carries no label, sent from
/// `paykit_request` or a labelled selling scope to an app holding `paykit_pay`. So `paykit_pay` to
/// `paykit_pay` and `paykit_request` to `paykit_request` are dropped, whatever the label.
///
/// Security: check 8 sees only the two certificates, not which side the sender is on; the
/// request record decides the side (Paykit wire 5.3). An app holding both roles may stand on
/// either side, so the check passes when either pairing holds.
fn check_cancellation_roles(label: Option<&str>, sender: &[String], own: &[String]) -> Result<()> {
    let holds_pay = |scopes: &[String]| scopes.iter().any(|s| s == "paykit_pay");
    let payer_cancels = holds_pay(sender) && reply_label_allowed(label, own);
    let payee_cancels = label.is_none() && scopes::acts_as_payee(sender) && holds_pay(own);
    if payer_cancels || payee_cancels {
        return Ok(());
    }
    invalid(
        "Paykit spec 3.3: a cancellation's recipient doesn't hold the other role than its sender",
    )
}

/// Whether an app holding `scopes` takes a payer's reply labelled `label` (Paykit spec 3.4, Paykit
/// wire 4.1 check 8): with a label, `paykit_request` or that label; without, `paykit_request`.
fn reply_label_allowed(label: Option<&str>, scopes: &[String]) -> bool {
    match label {
        Some(label) => switch_scopes::holds(scopes, &format!("{}:{label}", scopes::REQUEST)),
        None => scopes.iter().any(|s| s == "paykit_request"),
    }
}

/// Which side sent a cancellation (Paykit wire 4.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The payer.
    Payer,
    /// The payee.
    Payee,
}

/// The targets of a payer's reply and a payee's cancellation (Paykit spec 3.4, Paykit wire
/// 4.3 "Targets"): whether a sender wraps a message of `kind`, from `side`, about a request
/// labelled `label`, to an app holding `scopes`.
///
/// The payer wraps its acceptance, rejection, cancellation and proof about a labelled
/// request only to the payee's apps holding `paykit_request` or that label, and about an
/// unlabelled request, or a payment without one, only to those holding `paykit_request`. A payee's
/// cancellation carries no label and goes only to the payer's apps holding `paykit_pay`: a
/// cancellation's recipient holds the other role than its sender (Paykit spec 3.3), as
/// check 8 requires ([`check_kind_scopes`]). Every other kind goes to the apps that receive
/// it (Paykit spec 3.3). A resend answer follows the same targets (Paykit wire 4.12).
pub fn targets_app(kind: Kind, side: Side, label: Option<&Label>, scopes: &[String]) -> bool {
    if !holds_role(scopes, kind.recipients()) {
        return false;
    }
    match (kind, side) {
        (Kind::PaymentRequestCancellation, Side::Payee) => {
            label.is_none() && scopes.iter().any(|s| s == "paykit_pay")
        }
        (k, Side::Payer) if k.is_payer_reply() => match label {
            Some(label) => switch_scopes::holds(scopes, &label.scope()),
            None => scopes.iter().any(|s| s == "paykit_request"),
        },
        _ => true,
    }
}

/// The rules Paykit gives the Switch's recipient checks (Switch wire 5.1, Paykit wire 4.1):
/// the opaque members, the one kind sent between own apps (check 6) and the scopes of each
/// kind (check 8), for the receiving app whose certificate holds `own_scopes`.
#[derive(Clone, Debug)]
pub struct PaykitKindRules {
    own_scopes: Vec<String>,
}

impl PaykitKindRules {
    /// The rules for the receiving app, whose own certificate holds `own_scopes`.
    pub fn new(own_scopes: Vec<String>) -> Self {
        Self { own_scopes }
    }

    /// The rules for the receiving app with this certificate.
    pub fn for_certificate(own: &AppCert) -> Self {
        Self::new(own.scopes().to_vec())
    }

    /// The own certificate's scopes.
    pub fn own_scopes(&self) -> &[String] {
        &self.own_scopes
    }
}

impl KindRules for PaykitKindRules {
    fn opaque_members(&self) -> &[OpaqueMember<'_>] {
        OPAQUE_MEMBERS
    }

    fn sent_between_own_apps(&self, kind: &str) -> bool {
        Kind::from_kind(kind).is_some_and(Kind::sent_between_own_apps)
    }

    /// Check 8, after Paykit's verifiers' checks of the sender's certificate (Paykit wire
    /// 3.1): its scope rules and permissions; then [`check_kind_scopes`]. An
    /// `executor_notice` from another identity is refused: it goes only between own apps.
    fn check_scopes(&self, message: &Message, sender: &AppCert) -> pubky_switch_core::Result<()> {
        let result = scopes::check_certificate(sender)
            .and_then(|()| {
                if message.kind == Kind::ExecutorNotice.as_str() && !message.is_from_own_app() {
                    return invalid(
                        "Paykit wire 4.3: executor_notice goes only between apps of one identity",
                    );
                }
                Ok(())
            })
            .and_then(|()| check_kind_scopes(message, sender.scopes(), &self.own_scopes));
        result.map_err(to_switch)
    }
}

/// A Paykit refusal handed back through the Switch's [`KindRules`] interface.
fn to_switch(error: Error) -> pubky_switch_core::Error {
    match error {
        Error::Switch(inner) => inner,
        Error::Invalid(why) | Error::Validation(why) => pubky_switch_core::Error::Invalid(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_core::keys::{AppKeys, IdentityKey};
    use pubky_switch_core::message::Sender;
    use pubky_switch_core::records::AppCertBody;
    use pubky_switch_core::scopes::Permission;
    use serde_json::{json, Value};

    fn owned(scopes: &[&str]) -> Vec<String> {
        scopes.iter().map(|s| (*s).to_owned()).collect()
    }

    fn message(kind: &str, body: Value) -> Message {
        let Value::Object(body) = body else {
            unreachable!()
        };
        Message {
            kind: kind.into(),
            msg_id: "1b4e28ba-2fa1-41d2-883f-0016d3cca427".into(),
            created_at_ms: 0,
            from: Sender {
                identity: [1; 32],
                app_id: [2; 16],
            },
            to: [3; 32],
            refs: Vec::new(),
            body,
        }
    }

    #[test]
    fn test_kinds_round_trip_their_strings() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_kind(kind.as_str()), Some(kind));
            assert_eq!(
                MessageKind::parse(kind.as_str()).unwrap(),
                MessageKind::Paykit(kind)
            );
        }
        assert!(MessageKind::parse("paykit.other").is_err());
        assert!(matches!(
            MessageKind::parse("paykit-cap.offer").unwrap(),
            MessageKind::Extension(_)
        ));
    }

    #[test]
    fn test_kinds_extension_kind_syntax() {
        assert!(ExtensionKind::parse("paykit-cap.offer_2").is_ok());
        for bad in [
            "paykit-Cap.offer",
            "cap",
            "paykit-cap.",
            "-x.offer",
            "x-y.Offer",
            "paykit.offer",
            "cap.offer",
            "chat.message",
        ] {
            assert!(ExtensionKind::parse(bad).is_err(), "{bad}");
        }
        let long = format!("paykit-cap.{}", "a".repeat(65));
        assert!(ExtensionKind::parse(&long).is_err());
        assert!(matches!(
            ExtensionKind::new("cap.offer"),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            ExtensionKind::parse("cap.offer"),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn test_kinds_check_8_accepts_the_table() {
        let request = message("paykit.payment_request", json!({}));
        check_kind_scopes(
            &request,
            &owned(&["paykit_request"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        let labelled = message("paykit.payment_request", json!({ "label": "shop1" }));
        check_kind_scopes(
            &labelled,
            &owned(&["paykit_request:shop1"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        check_kind_scopes(
            &labelled,
            &owned(&["paykit_request"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        let notice = message("paykit.executor_notice", json!({}));
        check_kind_scopes(
            &notice,
            &owned(&["paykit_allowance"]),
            &owned(&["paykit_allowance_execute"]),
        )
        .unwrap();
        let resend = message("paykit.resend_request", json!({}));
        check_kind_scopes(
            &resend,
            &owned(&["paykit_contacts"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        let ext = message("paykit-cap.offer", json!({}));
        check_kind_scopes(
            &ext,
            &owned(&["paykit_ext:paykit-cap"]),
            &owned(&["paykit_ext:paykit-cap"]),
        )
        .unwrap();
    }

    #[test]
    fn test_kinds_check_8_rejects_each_rule() {
        let request = message("paykit.payment_request", json!({}));
        assert!(
            check_kind_scopes(&request, &owned(&["paykit_pay"]), &owned(&["paykit_pay"])).is_err()
        );
        assert!(check_kind_scopes(
            &request,
            &owned(&["paykit_request"]),
            &owned(&["paykit_request"])
        )
        .is_err());
        assert!(check_kind_scopes(
            &request,
            &owned(&["paykit_request:shop1"]),
            &owned(&["paykit_pay"])
        )
        .is_err());
        let other = message("paykit.payment_request", json!({ "label": "shop2" }));
        assert!(check_kind_scopes(
            &other,
            &owned(&["paykit_request:shop1"]),
            &owned(&["paykit_pay"])
        )
        .is_err());
        let ext = message("paykit-cap.offer", json!({}));
        assert!(check_kind_scopes(
            &ext,
            &owned(&["paykit_ext:paykit-cap"]),
            &owned(&["paykit_ext:paykit-other"])
        )
        .is_err());
        // A prefix without a `-` is no extension's: the kind is dropped, whatever the scopes.
        let other = message("cap.offer", json!({}));
        assert!(check_kind_scopes(
            &other,
            &owned(&["paykit_ext:cap"]),
            &owned(&["paykit_ext:cap"])
        )
        .is_err());
        let unknown = message("paykit.other", json!({}));
        assert!(
            check_kind_scopes(&unknown, &owned(&["paykit_pay"]), &owned(&["paykit_pay"])).is_err()
        );
    }

    #[test]
    fn test_kinds_reply_labels_narrow_recipients() {
        let labelled = message(
            "paykit.payment_request_acceptance",
            json!({ "label": "shop1" }),
        );
        check_kind_scopes(
            &labelled,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request:shop1"]),
        )
        .unwrap();
        check_kind_scopes(
            &labelled,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request"]),
        )
        .unwrap();
        assert!(check_kind_scopes(
            &labelled,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request:shop2"])
        )
        .is_err());
        let plain = message("paykit.payment_request_acceptance", json!({}));
        assert!(check_kind_scopes(
            &plain,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request:shop1"])
        )
        .is_err());
        let cancel = message("paykit.payment_request_cancellation", json!({}));
        check_kind_scopes(
            &cancel,
            &owned(&["paykit_request"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        assert!(check_kind_scopes(
            &cancel,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request:shop1"])
        )
        .is_err());
    }

    #[test]
    fn test_kinds_sender_side_of_check_8() {
        let labelled = message("paykit.payment_request", json!({ "label": "shop1" }));
        check_sender_scopes(&labelled, &owned(&["paykit_request:shop1"])).unwrap();
        check_sender_scopes(&labelled, &owned(&["paykit_request"])).unwrap();
        for sender in [
            &["paykit_request:shop2"][..],
            &["chat"][..],
            &["paykit_pay"][..],
        ] {
            assert!(
                check_sender_scopes(&labelled, &owned(sender)).is_err(),
                "{sender:?}"
            );
        }
        let cancel = message(
            "paykit.payment_request_cancellation",
            json!({ "label": "shop1" }),
        );
        check_sender_scopes(&cancel, &owned(&["paykit_pay"])).unwrap();
        assert!(check_sender_scopes(&cancel, &owned(&["paykit_request:shop1"])).is_err());
        let ext = message("paykit-cap.offer", json!({}));
        check_sender_scopes(&ext, &owned(&["paykit_ext:paykit-cap"])).unwrap();
        assert!(check_sender_scopes(&ext, &owned(&["paykit_request"])).is_err());
    }

    #[test]
    fn test_kinds_cancellation_goes_to_the_other_role() {
        let plain = message("paykit.payment_request_cancellation", json!({}));
        let labelled = message(
            "paykit.payment_request_cancellation",
            json!({ "label": "shop1" }),
        );
        // The payee's, to the payer's `paykit_pay`; the payer's, to the payee's `paykit_request` or
        // label.
        check_kind_scopes(&plain, &owned(&["paykit_request"]), &owned(&["paykit_pay"])).unwrap();
        check_kind_scopes(
            &plain,
            &owned(&["paykit_request:shop1"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        check_kind_scopes(&plain, &owned(&["paykit_pay"]), &owned(&["paykit_request"])).unwrap();
        check_kind_scopes(
            &labelled,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request:shop1"]),
        )
        .unwrap();
        check_kind_scopes(
            &labelled,
            &owned(&["paykit_pay"]),
            &owned(&["paykit_request"]),
        )
        .unwrap();
        // An app holding both roles may be on either side.
        check_kind_scopes(
            &plain,
            &owned(&["paykit_pay", "paykit_request"]),
            &owned(&["paykit_pay"]),
        )
        .unwrap();
        // The same role on both ends is dropped.
        for (sender, own) in [
            (&["paykit_pay"][..], &["paykit_pay"][..]),
            (&["paykit_request"][..], &["paykit_request"][..]),
            (&["paykit_request:shop1"][..], &["paykit_request"][..]),
            (&["paykit_request"][..], &["paykit_request:shop1"][..]),
        ] {
            let error = check_kind_scopes(&plain, &owned(sender), &owned(own)).unwrap_err();
            assert!(
                error.to_string().contains("other role"),
                "{sender:?} to {own:?}"
            );
        }
        // A labelled cancellation is the payer's: from `paykit_request`, or to `paykit_pay`, it is
        // dropped.
        assert!(check_kind_scopes(
            &labelled,
            &owned(&["paykit_request"]),
            &owned(&["paykit_pay"])
        )
        .is_err());
        assert!(
            check_kind_scopes(&labelled, &owned(&["paykit_pay"]), &owned(&["paykit_pay"])).is_err()
        );
        assert!(check_kind_scopes(
            &labelled,
            &owned(&["paykit_request:shop1"]),
            &owned(&["paykit_request"])
        )
        .is_err());
    }

    fn issued(identity: &IdentityKey, scopes: &[&str], permissions: &[Permission]) -> AppCert {
        let app = AppKeys::from_secrets([4; 32], [5; 32]);
        AppCert::issue(
            identity,
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

    #[test]
    fn test_kinds_rules_drop_paykit_kinds_from_another_protocol_only() {
        let identity = IdentityKey::from_secret([6; 32]);
        // A chat-only certificate is valid at Paykit's verifiers, but authorizes no Paykit
        // kind: its payment_request and its resend_request are dropped at check 8.
        let chat = issued(&identity, &["chat"], &[Permission::ListsRead]);
        crate::scopes::check_certificate(&chat).unwrap();
        let wallet = PaykitKindRules::new(owned(&["paykit_pay", "paykit_contacts"]));
        let request = message(
            "paykit.payment_request",
            json!({ "amount": { "asset": "btc", "value": "1" }, "payment_reference": "r",
                "proposal_expires_at": null }),
        );
        let error = wallet
            .check_scopes(&request, &chat)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Paykit spec 3.3"), "{error}");
        let resend = message("paykit.resend_request", json!({ "msg_ids": [] }));
        assert!(wallet.check_scopes(&resend, &chat).is_err());
        // The same kinds from a Paykit App that also chats pass.
        let shop = issued(
            &identity,
            &["paykit_request", "chat"],
            &[Permission::ListsRead, Permission::ConversationAdd],
        );
        wallet.check_scopes(&request, &shop).unwrap();
        wallet.check_scopes(&resend, &shop).unwrap();
        // An own app of another protocol only takes no Paykit kind either.
        assert!(check_kind_scopes(&resend, &owned(&["paykit_pay"]), &owned(&["chat"])).is_err());
    }

    #[test]
    fn test_kinds_rules_check_the_sender_certificate_and_own_app_kinds() {
        let identity = IdentityKey::from_secret([6; 32]);
        let good = issued(
            &identity,
            &["paykit_allowance"],
            &[Permission::ClaimsRead, Permission::ConversationAdd],
        );
        let rules = PaykitKindRules::new(owned(&[
            "paykit_allowance_execute",
            "paykit_allowance",
            "paykit_pay",
            "paykit_contacts",
        ]));
        let mut notice = message("paykit.executor_notice", json!({}));
        notice.from.identity = identity.public();
        notice.to = identity.public();
        rules.check_scopes(&notice, &good).unwrap();
        assert!(rules.sent_between_own_apps(Kind::ExecutorNotice.as_str()));
        assert!(!rules.sent_between_own_apps(Kind::PaymentRequest.as_str()));
        notice.to = [3; 32];
        assert!(
            rules.check_scopes(&notice, &good).is_err(),
            "from another identity"
        );
        notice.to = identity.public();
        let extra = issued(
            &identity,
            &["paykit_allowance"],
            &[
                Permission::ClaimsRead,
                Permission::ClaimsWrite,
                Permission::ConversationAdd,
            ],
        );
        assert!(
            rules.check_scopes(&notice, &extra).is_err(),
            "permissions aren't Paykit's list"
        );
        assert_eq!(rules.opaque_members(), OPAQUE_MEMBERS);
    }

    #[test]
    fn test_kinds_targets_follow_labels() {
        let shop1 = Label::parse("shop1").unwrap();
        let acc = Kind::PaymentRequestAcceptance;
        assert!(targets_app(
            acc,
            Side::Payer,
            Some(&shop1),
            &owned(&["paykit_request:shop1"])
        ));
        assert!(targets_app(
            acc,
            Side::Payer,
            Some(&shop1),
            &owned(&["paykit_request"])
        ));
        assert!(!targets_app(
            acc,
            Side::Payer,
            Some(&shop1),
            &owned(&["paykit_request:shop2"])
        ));
        assert!(!targets_app(
            acc,
            Side::Payer,
            None,
            &owned(&["paykit_request:shop1"])
        ));
        let cancel = Kind::PaymentRequestCancellation;
        assert!(targets_app(
            cancel,
            Side::Payee,
            None,
            &owned(&["paykit_pay"])
        ));
        assert!(targets_app(
            cancel,
            Side::Payee,
            None,
            &owned(&["paykit_pay", "paykit_request"])
        ));
        assert!(!targets_app(
            cancel,
            Side::Payee,
            None,
            &owned(&["paykit_request:shop1"])
        ));
        assert!(!targets_app(
            cancel,
            Side::Payer,
            None,
            &owned(&["paykit_pay"])
        ));
        assert!(targets_app(
            cancel,
            Side::Payer,
            None,
            &owned(&["paykit_request"])
        ));
        assert!(!targets_app(
            Kind::AllowanceProposal,
            Side::Payee,
            None,
            &owned(&["paykit_pay"])
        ));
    }

    #[test]
    fn test_kinds_cancellation_targets_the_other_role() {
        let cancel = Kind::PaymentRequestCancellation;
        let shop1 = Label::parse("shop1").unwrap();
        // The payee's goes only to the payer's apps holding `paykit_pay`, never to one holding only
        // `paykit_request`, which check 8 would drop.
        assert!(!targets_app(
            cancel,
            Side::Payee,
            None,
            &owned(&["paykit_request"])
        ));
        assert!(!targets_app(
            cancel,
            Side::Payee,
            Some(&shop1),
            &owned(&["paykit_pay"])
        ));
        // The payer's goes only to the payee's apps the label rule allows, never to `paykit_pay`.
        assert!(!targets_app(
            cancel,
            Side::Payer,
            None,
            &owned(&["paykit_pay"])
        ));
        assert!(targets_app(
            cancel,
            Side::Payer,
            Some(&shop1),
            &owned(&["paykit_request:shop1"])
        ));
        assert!(!targets_app(
            cancel,
            Side::Payer,
            Some(&shop1),
            &owned(&["paykit_pay"])
        ));
        // Every app a cancellation targets takes it at check 8, from a sender of that side.
        let plain = message("paykit.payment_request_cancellation", json!({}));
        let labelled = message(
            "paykit.payment_request_cancellation",
            json!({ "label": "shop1" }),
        );
        let scope_sets: [&[&str]; 6] = [
            &["paykit_pay"],
            &["paykit_request"],
            &["paykit_request:shop1"],
            &["paykit_pay", "paykit_request"],
            &["paykit_pay", "paykit_request:shop1"],
            &["paykit_contacts"],
        ];
        for own in scope_sets {
            let own = owned(own);
            for (side, sender, label, m) in [
                (Side::Payee, owned(&["paykit_request"]), None, &plain),
                (Side::Payee, owned(&["paykit_request:shop1"]), None, &plain),
                (Side::Payer, owned(&["paykit_pay"]), None, &plain),
                (Side::Payer, owned(&["paykit_pay"]), Some(&shop1), &labelled),
            ] {
                assert_eq!(
                    targets_app(cancel, side, label, &own),
                    check_kind_scopes(m, &sender, &own).is_ok(),
                    "{side:?} from {sender:?} to {own:?}"
                );
            }
        }
    }

    #[test]
    fn test_kinds_label_must_be_a_string() {
        let bad = message("paykit.payment_request", json!({ "label": 1 }));
        assert!(
            check_kind_scopes(&bad, &owned(&["paykit_request"]), &owned(&["paykit_pay"])).is_err()
        );
    }
}
