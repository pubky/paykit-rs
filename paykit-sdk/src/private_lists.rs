//! Private Payment List latest-state records.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Derived latest-state view of a counterparty's Private Payment List.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListView {
    /// Stream item id of the latest valid list.
    pub latest_stream_item_id: Option<u64>,
    /// Current endpoint payloads keyed by identifier string.
    pub payment_endpoints: HashMap<String, String>,
}
