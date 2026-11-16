//! Paykit's flows on the client's scripted switches: two identities, each on its own switch,
//! with Paykit apps joined from approvals and a fake rail.

// The helpers set up fixed test state, where a failure is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod allowances;
mod answer_state;
mod blocked;
mod blocked_return;
mod certificate;
mod common;
mod conversion;
mod crash;
mod custom_rail;
mod destinations;
mod discovery_state;
mod divergent;
mod erasure;
mod final_check;
mod labels;
mod local_state;
mod mixed_versions;
mod notices;
mod one_time;
mod outcome_record;
mod period_credit;
mod periods;
mod protocols;
mod pruning;
mod public;
mod receipts;
mod receive_faults;
mod recovery;
mod recovery_state;
mod recurring;
mod replies;
mod resend;
mod restore;
mod restore_hold;
mod spam;
mod stored;
mod unrequested;
mod wallets;
