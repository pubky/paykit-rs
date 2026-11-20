//! The original approving app's first executor grant (Paykit spec 8.3).

mod approval;
mod checkpoint;
mod evidence;
mod grant;
pub(crate) use checkpoint::ExecutorHistory;

/// The proposal must belong to the Allower's surviving history as of the grant.
fn in_grant_history(proposed: u64, grant_generation: u64, recovery_from: Option<u64>) -> bool {
    proposed > recovery_from.unwrap_or(0) && proposed <= grant_generation
}
