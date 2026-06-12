//! Recurring Payment Request scheduling records.

use serde::{Deserialize, Serialize};

/// Local scheduling state for a recurring Payment Request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchedulingState {
    /// No scheduled job exists.
    NotScheduled,
    /// A scheduled job exists.
    Scheduled,
    /// Scheduling is paused.
    Paused,
}
