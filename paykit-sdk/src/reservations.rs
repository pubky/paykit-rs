//! Endpoint reservation records.

use serde::{Deserialize, Serialize};

/// SDK state for an endpoint reservation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointReservationState {
    /// Reservation can still be used.
    Active,
    /// Reservation was used.
    Used,
    /// Reservation was rotated after use.
    Rotated,
    /// Reservation was retired.
    Retired,
}
