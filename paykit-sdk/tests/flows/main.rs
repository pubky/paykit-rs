//! Paykit's flows on the client's scripted switches: two identities, each on its own switch,
//! with Paykit apps joined from approvals and a fake rail.

#![recursion_limit = "256"]
// The helpers set up fixed test state, where a failure is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod allowance_executor;
mod allowances;
mod answer_state;
mod blocked;
mod blocked_return;
mod certificate;
mod common;
mod conversion;
mod crash;
mod custom_rail;
mod deletions;
mod destinations;
mod discovery_state;
mod divergent;
mod erasure;
mod extensions;
mod final_check;
mod held_back;
mod hold_confirmation;
mod labels;
mod list_reads;
mod local_state;
mod mixed_protocols;
mod mixed_versions;
mod notices;
mod offline_public;
mod one_time;
mod outcome_record;
mod own_record;
mod payment_time;
mod period_credit;
mod period_payments;
mod periods;
mod protocols;
mod provider_loss;
mod pruning;
mod public;
mod public_read;
mod publishing;
mod receipt_reader;
mod receipts;
mod receive_faults;
mod recovery;
mod recovery_reports;
mod recovery_state;
mod recurring;
mod replies;
mod resend;
mod restore;
mod restore_first_read;
mod restore_hold;
mod resume;
mod spam;
mod stored;
mod unrequested;
mod unrequested_claim;
mod wallets;
