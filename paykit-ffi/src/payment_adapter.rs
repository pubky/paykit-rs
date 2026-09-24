use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use paykit_sdk::{
    PaymentAdapter, PaymentTarget, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
    PrivatePaymentEndpointSelectionRequest, PrivateReceivingDetail, PubkyPublicKey,
    PublicPaymentEndpointCandidate, PublicPaymentEndpointSelectionRequest, PublicReceivingDetail,
};

use crate::{
    errors::{validation_error, PaykitFfiError},
    profiles::FfiPublicationStatus,
    sdk::FfiPaykitSdk,
    session::app_public_key,
};

mod conversions;

pub(crate) use conversions::payment_endpoint_reservation_from_parts;
use conversions::{
    payment_adapter_error, payment_adapter_unavailable, private_candidate_id, public_candidate_id,
    selected_candidates,
};

/// Payment adapter payload text with redacted debug output.
///
/// Android callback implementations should export callback-supplied payloads
/// and close their generated native wrappers before returning.
#[derive(uniffi::Object)]
pub struct FfiPaymentPayload {
    text: String,
}

impl fmt::Debug for FfiPaymentPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FfiPaymentPayload(<redacted:{} bytes>)", self.text.len())
    }
}

#[uniffi::export]
impl FfiPaymentPayload {
    /// Create a payment payload from adapter-owned text.
    #[uniffi::constructor]
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Export the payload text for payment adapter execution.
    pub fn export_text(&self) -> String {
        self.text.clone()
    }
}

/// Reservation attribution metadata with redacted debug output.
///
/// Android callback implementations should export callback-supplied metadata
/// and close its generated native wrapper before returning.
#[derive(uniffi::Object)]
pub struct FfiReservationAttribution {
    fields: HashMap<String, String>,
}

impl fmt::Debug for FfiReservationAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FfiReservationAttribution(<redacted:{} fields>)",
            self.fields.len()
        )
    }
}

#[uniffi::export]
impl FfiReservationAttribution {
    /// Create reservation attribution metadata.
    #[uniffi::constructor]
    pub fn new(fields: HashMap<String, String>) -> Self {
        Self { fields }
    }

    /// Export attribution fields for payment adapter cleanup.
    pub fn export_fields(&self) -> HashMap<String, String> {
        self.fields.clone()
    }
}

/// Payment-method-specific receiving detail for public publication.
#[derive(uniffi::Record, Clone)]
pub struct FfiPublicReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Payment-method-specific receiving detail for a Private Payment List.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivateReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Private receiving detail reserved by the payment adapter.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivatePaymentEndpointReservation {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Reserved receiving detail.
    pub receiving_detail: FfiPrivateReceivingDetail,
    /// Optional reservation expiry as RFC3339 text.
    pub expires_at: Option<String>,
    /// Adapter attribution metadata.
    pub attribution: Arc<FfiReservationAttribution>,
}

impl fmt::Debug for FfiPrivatePaymentEndpointReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPrivatePaymentEndpointReservation")
            .field("reservation_id", &"<redacted>")
            .field("receiving_detail", &self.receiving_detail)
            .field("expires_at", &self.expires_at)
            .field("attribution", &self.attribution)
            .finish()
    }
}

/// Reservation callback result kind.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPrivateReceivingDetailReservationResponseKind {
    /// Use `current_private_receiving_details` for this private list.
    UseCurrentReceivingDetails,
    /// Use the reservations carried by this response, including an empty list.
    Reservations,
    /// Reserved invalid response kind.
    Unknown,
}

/// Explicit result for private receiving-detail reservation callbacks.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivateReceivingDetailReservationResponse {
    /// Response kind.
    pub kind: FfiPrivateReceivingDetailReservationResponseKind,
    /// Reserved details when `kind` is `Reservations`.
    pub reservations: Vec<FfiPrivatePaymentEndpointReservation>,
}

/// Request passed to cancel a receiving-detail reservation.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivatePaymentEndpointReservationCancellation {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Counterparty the reservation was intended for.
    pub counterparty: String,
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Hash of the reserved endpoint payload.
    pub payload_hash: String,
    /// Adapter attribution metadata from the reservation.
    pub attribution: Arc<FfiReservationAttribution>,
}

impl fmt::Debug for FfiPrivatePaymentEndpointReservationCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPrivatePaymentEndpointReservationCancellation")
            .field("reservation_id", &"<redacted>")
            .field("counterparty", &self.counterparty)
            .field("identifier", &self.identifier)
            .field("payload_hash", &"<redacted>")
            .field("attribution", &self.attribution)
            .finish()
    }
}

/// Optional amount context for endpoint selection.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiPaymentAmountContext {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Public Payment Endpoint candidate passed to the payment adapter.
#[derive(uniffi::Record, Clone)]
pub struct FfiPublicPaymentEndpointCandidate {
    /// Opaque candidate id for this callback request.
    pub candidate_id: String,
    /// Counterparty that published the endpoint.
    pub counterparty: String,
    /// Application that published the endpoint.
    pub app_id: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Private Payment Endpoint candidate passed to the payment adapter.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivatePaymentEndpointCandidate {
    /// Opaque candidate id for this callback request.
    pub candidate_id: String,
    /// Counterparty that privately shared the endpoint.
    pub counterparty: String,
    /// Application that privately shared the endpoint.
    pub app_id: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Request passed to the payment adapter for public endpoint ordering.
#[derive(uniffi::Record, Clone)]
pub struct FfiPublicPaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: String,
    /// Optional amount context.
    pub amount: Option<FfiPaymentAmountContext>,
    /// Public candidate endpoints in SDK preference order.
    pub candidates: Vec<FfiPublicPaymentEndpointCandidate>,
}

/// Request passed to the payment adapter for private endpoint ordering.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivatePaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: String,
    /// Optional amount context.
    pub amount: Option<FfiPaymentAmountContext>,
    /// Private candidate endpoints in SDK preference order.
    pub candidates: Vec<FfiPrivatePaymentEndpointCandidate>,
}

/// Payment-method-specific execution payload produced by the adapter.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentTarget {
    /// Method-specific target payload.
    pub payload: Arc<FfiPaymentPayload>,
}

impl_redacted_debug!(
    FfiPublicReceivingDetail,
    FfiPrivateReceivingDetail,
    FfiPrivateReceivingDetailReservationResponse,
    FfiPaymentAmountContext,
    FfiPublicPaymentEndpointCandidate,
    FfiPrivatePaymentEndpointCandidate,
    FfiPublicPaymentEndpointSelectionRequest,
    FfiPrivatePaymentEndpointSelectionRequest,
    FfiPaymentTarget,
);

/// One public endpoint changed during sync.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiEndpointSyncChange {
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Resulting local publication status.
    pub status: FfiPublicationStatus,
    /// Error text for failed changes.
    pub error: Option<String>,
}

/// Summary returned after public Payment Endpoint sync.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiEndpointSyncReport {
    /// Endpoints successfully published or updated.
    pub published: Vec<FfiEndpointSyncChange>,
    /// Endpoints successfully removed.
    pub removed: Vec<FfiEndpointSyncChange>,
    /// Endpoints that failed to publish or remove.
    pub failed: Vec<FfiEndpointSyncChange>,
}

/// Platform-owned, mode-specific payment adapter callbacks.
///
/// Public callbacks never receive private values, and private callbacks never
/// receive public values.
///
/// Callbacks must not synchronously call back into the same SDK handle while
/// the originating SDK call is waiting for them.
#[uniffi::export(with_foreign)]
pub trait FfiSdkPaymentAdapter: Send + Sync {
    /// Return receiving details intended for public Payment Endpoints.
    fn current_public_receiving_details(
        &self,
    ) -> Result<Vec<FfiPublicReceivingDetail>, PaykitFfiError>;

    /// Return receiving details for one counterparty's Private Payment List.
    fn current_private_receiving_details(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiPrivateReceivingDetail>, PaykitFfiError>;

    /// Reserve receiving details for a counterparty's Private Payment List.
    fn reserve_private_receiving_details(
        &self,
        counterparty: String,
    ) -> Result<FfiPrivateReceivingDetailReservationResponse, PaykitFfiError>;

    /// Cancel a previously reserved receiving detail.
    fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: FfiPrivatePaymentEndpointReservationCancellation,
    ) -> Result<(), PaykitFfiError>;

    /// Return payable public candidate ids in adapter-preferred order.
    fn select_public_payment_endpoint_ids(
        &self,
        request: FfiPublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError>;

    /// Build a payment target from a payable public endpoint.
    fn build_public_payment_target(
        &self,
        endpoint: FfiPublicPaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError>;

    /// Return payable private candidate ids in adapter-preferred order.
    fn select_private_payment_endpoint_ids(
        &self,
        request: FfiPrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError>;

    /// Build a payment target from a payable private endpoint.
    fn build_private_payment_target(
        &self,
        endpoint: FfiPrivatePaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError>;
}

#[derive(Clone)]
pub(crate) struct FfiSdkPaymentAdapterAdapter {
    pub(crate) adapter: Arc<dyn FfiSdkPaymentAdapter>,
}

#[async_trait]
impl PaymentAdapter for FfiSdkPaymentAdapterAdapter {
    async fn current_public_receiving_details(
        &self,
    ) -> paykit_sdk::Result<Vec<PublicReceivingDetail>> {
        self.adapter
            .current_public_receiving_details()
            .map_err(|err| payment_adapter_error(err, "load public receiving details"))?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn current_private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> paykit_sdk::Result<Vec<PrivateReceivingDetail>> {
        self.adapter
            .current_private_receiving_details(app_public_key(counterparty))
            .map_err(|err| payment_adapter_error(err, "load private receiving details"))?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn reserve_private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> paykit_sdk::Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        self.adapter
            .reserve_private_receiving_details(app_public_key(counterparty))
            .map_err(|err| payment_adapter_error(err, "reserve private receiving details"))?
            .try_into()
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> paykit_sdk::Result<()> {
        self.adapter
            .cancel_private_receiving_detail_reservation(cancellation.clone().into())
            .map_err(|err| {
                payment_adapter_error(err, "cancel private receiving detail reservation")
            })
    }

    async fn select_public_payment_endpoints(
        &self,
        request: &PublicPaymentEndpointSelectionRequest,
    ) -> paykit_sdk::Result<Vec<PublicPaymentEndpointCandidate>> {
        let mut seen_candidate_ids = HashSet::new();
        let mut candidates_by_id = Vec::with_capacity(request.candidates.len());
        for candidate in &request.candidates {
            let id = public_candidate_id(candidate);
            if !seen_candidate_ids.insert(id.clone()) {
                return Err(payment_adapter_error(
                    validation_error("duplicate public payment endpoint candidate"),
                    "select public payment endpoints",
                ));
            }
            candidates_by_id.push((
                id.clone(),
                FfiPublicPaymentEndpointCandidate::from_candidate(candidate, id),
            ));
        }
        let ffi_request = FfiPublicPaymentEndpointSelectionRequest {
            counterparty: app_public_key(&request.counterparty),
            amount: request.amount.clone().map(Into::into),
            candidates: candidates_by_id
                .iter()
                .map(|(_, candidate)| candidate.clone())
                .collect(),
        };
        let selected_ids = self
            .adapter
            .select_public_payment_endpoint_ids(ffi_request)
            .map_err(|err| payment_adapter_error(err, "select public payment endpoints"))?;

        selected_candidates(
            selected_ids,
            &candidates_by_id,
            &request.candidates,
            "select public payment endpoints",
        )
    }

    async fn build_public_payment_target(
        &self,
        endpoint: &PublicPaymentEndpointCandidate,
    ) -> paykit_sdk::Result<PaymentTarget> {
        self.adapter
            .build_public_payment_target(FfiPublicPaymentEndpointCandidate::from_candidate(
                endpoint,
                public_candidate_id(endpoint),
            ))
            .map_err(|err| payment_adapter_error(err, "build public payment target"))
            .map(Into::into)
    }

    async fn select_private_payment_endpoints(
        &self,
        request: &PrivatePaymentEndpointSelectionRequest,
    ) -> paykit_sdk::Result<Vec<PrivatePaymentEndpointCandidate>> {
        let mut seen_candidate_ids = HashSet::new();
        let mut candidates_by_id = Vec::with_capacity(request.candidates.len());
        for candidate in &request.candidates {
            let id = private_candidate_id(candidate);
            if !seen_candidate_ids.insert(id.clone()) {
                return Err(payment_adapter_error(
                    validation_error("duplicate private payment endpoint candidate"),
                    "select private payment endpoints",
                ));
            }
            candidates_by_id.push((
                id.clone(),
                FfiPrivatePaymentEndpointCandidate::from_candidate(candidate, id),
            ));
        }
        let ffi_request = FfiPrivatePaymentEndpointSelectionRequest {
            counterparty: app_public_key(&request.counterparty),
            amount: request.amount.clone().map(Into::into),
            candidates: candidates_by_id
                .iter()
                .map(|(_, candidate)| candidate.clone())
                .collect(),
        };
        let selected_ids = self
            .adapter
            .select_private_payment_endpoint_ids(ffi_request)
            .map_err(|err| payment_adapter_error(err, "select private payment endpoints"))?;

        selected_candidates(
            selected_ids,
            &candidates_by_id,
            &request.candidates,
            "select private payment endpoints",
        )
    }

    async fn build_private_payment_target(
        &self,
        endpoint: &PrivatePaymentEndpointCandidate,
    ) -> paykit_sdk::Result<PaymentTarget> {
        self.adapter
            .build_private_payment_target(FfiPrivatePaymentEndpointCandidate::from_candidate(
                endpoint,
                private_candidate_id(endpoint),
            ))
            .map_err(|err| payment_adapter_error(err, "build private payment target"))
            .map(Into::into)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FfiNoopSdkPaymentAdapter;

impl FfiSdkPaymentAdapter for FfiNoopSdkPaymentAdapter {
    fn current_public_receiving_details(
        &self,
    ) -> Result<Vec<FfiPublicReceivingDetail>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn current_private_receiving_details(
        &self,
        _counterparty: String,
    ) -> Result<Vec<FfiPrivateReceivingDetail>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn reserve_private_receiving_details(
        &self,
        _counterparty: String,
    ) -> Result<FfiPrivateReceivingDetailReservationResponse, PaykitFfiError> {
        Ok(FfiPrivateReceivingDetailReservationResponse {
            kind: FfiPrivateReceivingDetailReservationResponseKind::UseCurrentReceivingDetails,
            reservations: Vec::new(),
        })
    }

    fn cancel_private_receiving_detail_reservation(
        &self,
        _cancellation: FfiPrivatePaymentEndpointReservationCancellation,
    ) -> Result<(), PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn select_public_payment_endpoint_ids(
        &self,
        _request: FfiPublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn build_public_payment_target(
        &self,
        _endpoint: FfiPublicPaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn select_private_payment_endpoint_ids(
        &self,
        _request: FfiPrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn build_private_payment_target(
        &self,
        _endpoint: FfiPrivatePaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Publish current public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints(&self) -> Result<FfiEndpointSyncReport, PaykitFfiError> {
        self.runtime
            .sync_public_endpoints()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Publish explicit public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints_with_receiving_details(
        &self,
        receiving_details: Vec<FfiPublicReceivingDetail>,
    ) -> Result<FfiEndpointSyncReport, PaykitFfiError> {
        let receiving_details = receiving_details
            .into_iter()
            .map(TryInto::try_into)
            .collect::<paykit_sdk::Result<Vec<_>>>()?;
        self.runtime
            .sync_public_endpoints_with_receiving_details(receiving_details)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod redaction_tests {
    use super::*;

    fn assert_redacted<T: fmt::Debug>(value: &T, type_name: &str) {
        assert_eq!(format!("{value:?}"), format!("{type_name}(<redacted>)"));
    }

    #[test]
    fn test_payment_adapter_records_redact_debug() {
        let payload = Arc::new(FfiPaymentPayload::new("payload-secret".into()));
        let amount = FfiPaymentAmountContext {
            value: "amount-secret".into(),
            asset: "asset-secret".into(),
        };
        let public_detail = FfiPublicReceivingDetail {
            identifier: "public-identifier-secret".into(),
            payload: Arc::clone(&payload),
        };
        let private_detail = FfiPrivateReceivingDetail {
            identifier: "private-identifier-secret".into(),
            payload: Arc::clone(&payload),
        };
        let public_candidate = FfiPublicPaymentEndpointCandidate {
            candidate_id: "public-candidate-secret".into(),
            counterparty: "public-counterparty-secret".into(),
            app_id: "public-app-secret".into(),
            identifier: "public-identifier-secret".into(),
            payload: Arc::clone(&payload),
        };
        let private_candidate = FfiPrivatePaymentEndpointCandidate {
            candidate_id: "private-candidate-secret".into(),
            counterparty: "private-counterparty-secret".into(),
            app_id: "private-app-secret".into(),
            identifier: "private-identifier-secret".into(),
            payload: Arc::clone(&payload),
        };
        let public_request = FfiPublicPaymentEndpointSelectionRequest {
            counterparty: "public-request-secret".into(),
            amount: Some(amount.clone()),
            candidates: vec![public_candidate.clone()],
        };
        let private_request = FfiPrivatePaymentEndpointSelectionRequest {
            counterparty: "private-request-secret".into(),
            amount: Some(amount.clone()),
            candidates: vec![private_candidate.clone()],
        };
        let reservation_response = FfiPrivateReceivingDetailReservationResponse {
            kind: FfiPrivateReceivingDetailReservationResponseKind::Reservations,
            reservations: Vec::new(),
        };
        let target = FfiPaymentTarget { payload };

        assert_redacted(&public_detail, "FfiPublicReceivingDetail");
        assert_redacted(&private_detail, "FfiPrivateReceivingDetail");
        assert_redacted(
            &reservation_response,
            "FfiPrivateReceivingDetailReservationResponse",
        );
        assert_redacted(&amount, "FfiPaymentAmountContext");
        assert_redacted(&public_candidate, "FfiPublicPaymentEndpointCandidate");
        assert_redacted(&private_candidate, "FfiPrivatePaymentEndpointCandidate");
        assert_redacted(&public_request, "FfiPublicPaymentEndpointSelectionRequest");
        assert_redacted(
            &private_request,
            "FfiPrivatePaymentEndpointSelectionRequest",
        );
        assert_redacted(&target, "FfiPaymentTarget");
    }

    #[test]
    fn test_reservation_cancellation_debug_redacts_sensitive_fields() {
        let cancellation = FfiPrivatePaymentEndpointReservationCancellation {
            reservation_id: "reservation-id-secret".into(),
            counterparty: "pubky-counterparty".into(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: "payload-hash-secret".into(),
            attribution: Arc::new(FfiReservationAttribution::new(HashMap::from([(
                "payment_hash".into(),
                "attribution-secret".into(),
            )]))),
        };

        let debug = format!("{cancellation:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("reservation-id-secret"));
        assert!(!debug.contains("payload-hash-secret"));
        assert!(!debug.contains("attribution-secret"));
    }
}
