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
//! Recurring requests, Allowances with executors and ledgers, and publishing Private Payment
//! Lists build on the same parts, in their own modules.

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
mod pay;
mod paykit;
mod protocol;
pub mod rail;
mod receive;
mod reconcile;
mod records;
mod requests;
mod resend;
mod state;
#[cfg(test)]
mod testkit;
mod timer;

pub use activity::Role;
pub use approval::{AppScopes, RequestedScopes};
pub use destination::IDENTITY_GRACE_MS;
pub use error::{PaykitError, Refusal, Result};
pub use journal::{PaymentView, Stage};
pub use pay::{PayOptions, PayOutcome, PaymentMode, UnrequestedPayment, UNREQUESTED_DEADLINE_MS};
pub use paykit::{
    orphan_floor_s, Paykit, PaykitBackup, PaykitConfig, ReceiveSummary, DEFAULT_RESEND_BUDGET,
    PRE_DISPATCH_BUDGET_MS,
};
pub use protocol::{PaykitProtocol, HANDLED_KINDS};
pub use rail::{
    CandidateError, CandidateId, EndpointSelection, PaymentEndpointCandidate, Rail,
    RailDeclaration, RailError, RailOutcome, RailPaid, RailPayment, Transfer,
};
pub use receive::{HeldList, LISTS_REFRESH_MS};
pub use reconcile::{Doubtful, ReconcileReport, Resolution};
pub use requests::{
    PayeeActivity, PayeeStatus, RequestDraft, RequestRef, RequestStatus, RequestView, SentRequest,
};
pub use resend::{ResendSelection, RESEND_COOLDOWN_MS};
pub use timer::Timer;
