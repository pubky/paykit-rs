//! Paykit's flows on the client's scripted switches: two identities, each with Paykit apps
//! joined from approvals, a fake rail, and a bridge between the switches.

// The helpers set up fixed test state, where a failure is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
mod crash;
mod destinations;
mod labels;
mod one_time;
mod resend;
mod restore;
mod unrequested;
mod wallets;
