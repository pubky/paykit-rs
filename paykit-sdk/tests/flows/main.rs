//! Paykit's flows on the client's scripted switches: two identities, each on its own switch,
//! with Paykit apps joined from approvals and a fake rail.

// The helpers set up fixed test state, where a failure is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod allowances;
mod blocked;
mod blocked_return;
mod certificate;
mod common;
mod crash;
mod custom_rail;
mod destinations;
mod divergent;
mod erasure;
mod executor;
mod final_check;
mod handoff;
mod labels;
mod ledger;
mod local_state;
mod mixed_versions;
mod notices;
mod one_time;
mod outcome_record;
mod periods;
mod protocols;
mod pruning;
mod public;
mod receipts;
mod receive_faults;
mod recovery;
mod recurring;
mod repair;
mod replies;
mod resend;
mod restore;
mod restore_hold;
mod spam;
mod stored;
mod unrequested;
mod wallets;
