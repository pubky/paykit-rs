//! # paykit-sdk
//!
//! The Paykit v0.2 app SDK, for wallets and seller apps.
//!
//! This crate holds Paykit's payment flows and their state: requests and their answers,
//! proofs and receipts, attempts on claims, payments without a request, receiving and
//! re-sends, labelled selling scopes, reconciliation, and the scopes an app asks for at
//! approval. Payment rails stay outside Paykit, behind a narrow rail interface ([`Rail`]).
//!
//! It builds on `pubky-switch-client` for the Switch's side, and on `paykit-lib` for
//! Paykit's formats and rules. The protocol is defined by the Paykit spec and wire format in
//! `specs/v0.2/`.
//!
//! - [`PaykitProtocol`]: Paykit as the client's `Protocol`: scopes and kind rules, receiving
//!   (each message's record write committed with the cursor move), collections and their
//!   policies, and record marks.
//! - [`Paykit`]: one Paykit App, on the client's `App`, with Paykit's API: requests
//!   ([`Paykit::send_request`], [`Paykit::reject_request`], [`Paykit::confirm_payment`]),
//!   recurring requests' subscriptions and Billing Periods
//!   ([`Paykit::accept_subscription`], [`Paykit::send_period_details`]),
//!   paying ([`Paykit::pay_request`], [`Paykit::pay_without_request`],
//!   [`Paykit::resume_payments`]), receiving ([`Paykit::receive`]) and reconciliation
//!   ([`Paykit::reconcile`]).
//! - [`Rail`]: the payment method's interface; `rail::FakeRail` for tests (feature
//!   `test-support`).
//! - [`Timer`]: the host's timer, which bounds the wait for a payee's switch.
//! - [`AppScopes`]: Paykit's side of approval.
//! - [`app_records`]: Paykit's member of app records, and reading a payee's public
//!   endpoints from them ([`Paykit::public_endpoints`]).
//!
//! Paying recurring requests' periods, Allowances with executors and ledgers, and publishing
//! Private Payment Lists build on the same parts, in their own modules.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod activity;
pub mod app_records;
mod approval;
mod destination;
mod error;
mod hold;
mod journal;
mod marks;
mod notices;
mod pay;
mod paykit;
mod periods;
mod protocol;
pub mod rail;
mod receipts;
mod receive;
mod reconcile;
mod records;
mod requests;
mod resend;
mod state;
mod stored;
#[cfg(test)]
mod testkit;
mod timer;

pub use activity::Role;
pub use approval::{AppScopes, RequestedScopes};
pub use destination::IDENTITY_GRACE_MS;
pub use error::{PaykitError, Refusal, Result};
pub use journal::{PaymentView, Stage};
pub use marks::MAX_DROPPED_MARKS;
pub use notices::{Notice, NoticeKind};
pub use pay::{
    BlockedPayment, PayOptions, PayOutcome, PaymentDoubt, PaymentMode, TransferQuote,
    UnrequestedPayment, UNREQUESTED_DEADLINE_MS,
};
pub use paykit::{
    orphan_floor_s, Paykit, PaykitBackup, PaykitConfig, ReceiveSummary, DEFAULT_RESEND_BUDGET,
    PRE_DISPATCH_BUDGET_MS,
};
pub use periods::{PeriodDetailsView, PeriodView};
pub use protocol::{PaykitProtocol, HANDLED_KINDS};
pub use rail::{
    CandidateError, CandidateId, EndpointSelection, PaymentEndpointCandidate, Rail,
    RailDeclaration, RailError, RailOutcome, RailPaid, RailPayment, Transfer,
};
pub use receipts::{ReceiptDetails, ReceiptsOf, ReceivedReceipt};
pub use receive::{
    HeldList, KEPT_MESSAGE_MS, LISTS_REFRESH_MS, MAX_KEPT_REPLIES, MAX_KEPT_REPLIES_PER_SENDER,
};
pub use reconcile::{Doubtful, ReconcileReport, Resolution};
pub use requests::{
    AcceptOptions, MessageDelivery, PayeeActivity, PayeeStatus, RequestDraft, RequestRef,
    RequestStatus, RequestView, SentMessage, SentRequest,
};
pub use resend::{ResendSelection, RESEND_COOLDOWN_MS};
pub use timer::Timer;

#[cfg(test)]
mod tests {
    use paykit_lib::bodies::RequestDestination;
    use paykit_lib::{
        Asset, AttemptId, Decimal, PaymentAmount, PaymentEndpointIdentifier,
        PaymentEndpointPayload, PaymentReference,
    };
    use serde_json::{json, Map, Value};

    use crate::rail::{CandidateId, EndpointSelection, PaymentEndpointCandidate, ProofCheck};
    use crate::*;

    /// A string no `Debug` of payment data may show.
    const SECRET: &str = "s3cret-sentinel";

    fn btc() -> PaymentAmount {
        PaymentAmount {
            value: Decimal::new("1").unwrap(),
            asset: Asset::new("btc").unwrap(),
        }
    }

    fn id() -> PaymentEndpointIdentifier {
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()
    }

    fn payload() -> PaymentEndpointPayload {
        PaymentEndpointPayload::new(&format!("lnbc-{SECRET}")).unwrap()
    }

    fn proof() -> Map<String, Value> {
        json!({ "type": "x-ledger", "entry": SECRET })
            .as_object()
            .cloned()
            .unwrap()
    }

    #[test]
    fn test_debug_never_shows_payment_data() {
        let paid = RailPaid::new(proof()).with_paid_at_ms(1);
        let quote = TransferQuote {
            payee: [1; 32],
            app_id: [2; 16],
            endpoint_id: id(),
            payload_hash: pubky_switch_core::enc::hash(payload().as_str().as_bytes()),
            amount: btc(),
            transfer: Transfer::new(Some(btc()), None),
        };
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(id(), payload());
        let mut metadata = Map::new();
        metadata.insert("order".into(), Value::String(SECRET.into()));
        let draft = RequestDraft::new(
            btc(),
            PaymentReference::new(SECRET).unwrap(),
            RequestDestination::Endpoints(endpoints),
        )
        .with_metadata(metadata);
        let shown = [
            format!("{paid:?}"),
            format!("{:?}", RailOutcome::Paid(paid.clone())),
            format!(
                "{:?}",
                RailPayment {
                    endpoint_id: id(),
                    payload: payload(),
                    amount: btc(),
                    idempotency_key: AttemptId::from_bytes([3; 16]),
                    payment_reference: Some(SECRET.into()),
                }
            ),
            format!(
                "{:?}",
                ProofCheck {
                    endpoint_id: id(),
                    payload: Some(payload()),
                    amount: btc(),
                    attempt_id: AttemptId::from_bytes([3; 16]),
                    proof: proof(),
                }
            ),
            format!(
                "{:?}",
                PaymentEndpointCandidate::new(CandidateId::new(0), id(), payload())
            ),
            format!(
                "{:?}",
                EndpointSelection::new(vec![CandidateId::new(0)])
                    .with_error(CandidateId::new(1), RailError::Unavailable("down".into()))
            ),
            format!("{draft:?}"),
            format!(
                "{:?}",
                ReceiptDetails::new()
                    .with_metadata(proof())
                    .with_payment_reference(PaymentReference::new(SECRET).unwrap())
            ),
            format!(
                "{:?}",
                RequestView {
                    request: RequestRef {
                        payee: [1; 32],
                        request_hash: [4; 32],
                    },
                    record: [5; 32],
                    amount: btc(),
                    payment_reference: SECRET.into(),
                    label: None,
                    proposal_expires_at_ms: None,
                    recurring: false,
                    recurrence: None,
                    status: RequestStatus::Open,
                    receipt: Some(ReceivedReceipt {
                        record: [5; 32],
                        payee: [1; 32],
                        request_hash: Some([4; 32]),
                        billing_period: None,
                        attempt_id: AttemptId::from_bytes([3; 16]),
                        msg_id: "m".into(),
                        accepted_at_ms: None,
                        metadata: Some(proof()),
                        payment_reference: Some(SECRET.into()),
                    }),
                    periods: Vec::new(),
                }
            ),
            format!(
                "{:?}",
                app_records::PublicEndpoint {
                    app_id: [2; 16],
                    endpoint_id: id(),
                    payload: payload(),
                }
            ),
            format!(
                "{:?}",
                UnrequestedPayment::new([1; 32], btc()).with_options(
                    PayOptions::default()
                        .with_endpoint(id())
                        .with_transfer_confirmed(quote.clone())
                )
            ),
            format!(
                "{:?}",
                PaykitError::from(Refusal::Transfer {
                    reason: "another amount".into(),
                    quote: Box::new(quote),
                })
            ),
            format!(
                "{:?}",
                crate::journal::Destination::new(
                    [2; 16],
                    &id(),
                    &payload(),
                    paykit_lib::records::DestinationSource::List,
                    Some(1),
                    true,
                )
            ),
        ];
        for text in shown {
            assert!(!text.contains(SECRET), "{text}");
        }
    }
}
