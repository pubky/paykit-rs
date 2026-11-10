//! Paykit's flows on the client's scripted switches: two identities, each on its own switch,
//! with Paykit apps joined from approvals and a fake rail.

// The helpers set up fixed test state, where a failure is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
mod crash;
mod destinations;
mod labels;
mod one_time;
mod protocols;
mod recovery;
mod resend;
mod restore;
mod unrequested;
mod wallets;
