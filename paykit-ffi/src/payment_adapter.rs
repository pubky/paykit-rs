use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use paykit_sdk::{
    EndpointSyncChange, EndpointSyncReport, PaykitReceiverCapabilities, PaykitReceiverMarker,
    PaymentAdapter, PaymentAmountContext, PaymentTarget, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
    PrivatePaymentEndpointSelectionRequest, PrivateReceivingDetail, PubkyPublicKey,
    PublicPaymentEndpointCandidate, PublicPaymentEndpointSelectionRequest, PublicReceivingDetail,
};
use sha2::{Digest, Sha256};

use crate::{
    errors::{validation_error, PaykitFfiError},
    profiles::FfiPublicationStatus,
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key},
};

/// Payment adapter payload text with redacted debug output.
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
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPublicReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Payment-method-specific receiving detail for a Private Payment List.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivateReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Private receiving detail reserved by the payment adapter.
#[derive(uniffi::Record, Clone, Debug)]
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
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivateReceivingDetailReservationResponse {
    /// Response kind.
    pub kind: FfiPrivateReceivingDetailReservationResponseKind,
    /// Reserved details when `kind` is `Reservations`.
    pub reservations: Vec<FfiPrivatePaymentEndpointReservation>,
}

/// Request passed to cancel a receiving-detail reservation.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentEndpointReservationCancellation {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Counterparty the reservation was intended for.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Hash of the reserved endpoint payload.
    pub payload_hash: String,
    /// Adapter attribution metadata from the reservation.
    pub attribution: Arc<FfiReservationAttribution>,
}

/// Optional amount context for endpoint selection.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaymentAmountContext {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Public Payment Endpoint candidate passed to the payment adapter.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPublicPaymentEndpointCandidate {
    /// Opaque candidate id for this callback request.
    pub candidate_id: String,
    /// Counterparty that published the endpoint.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Private Payment Endpoint candidate passed to the payment adapter.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentEndpointCandidate {
    /// Opaque candidate id for this callback request.
    pub candidate_id: String,
    /// Counterparty that privately shared the endpoint.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Request passed to the payment adapter for public endpoint ordering.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPublicPaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Optional amount context.
    pub amount: Option<FfiPaymentAmountContext>,
    /// Public candidate endpoints in SDK preference order.
    pub candidates: Vec<FfiPublicPaymentEndpointCandidate>,
}

/// Request passed to the payment adapter for private endpoint ordering.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Optional amount context.
    pub amount: Option<FfiPaymentAmountContext>,
    /// Private candidate endpoints in SDK preference order.
    pub candidates: Vec<FfiPrivatePaymentEndpointCandidate>,
}

/// Payment-method-specific execution payload produced by the adapter.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentTarget {
    /// Method-specific target payload.
    pub payload: Arc<FfiPaymentPayload>,
}

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

/// Public capabilities advertised by a Paykit receiver marker.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitReceiverCapabilities {
    /// Receiver can participate in private Paykit payment workflows.
    pub private_payments: bool,
    /// Receiver can send or receive Payment Request messages.
    pub payment_requests: bool,
    /// Receiver can issue or retrieve Paykit Receipts.
    pub receipts: bool,
    /// Receiver can execute outgoing payments itself.
    pub outgoing_payments: bool,
}

/// Lightweight public marker for one Paykit receiver path.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitReceiverMarker {
    /// Receiver path this marker belongs to.
    pub receiver_path: String,
    /// Public receiver capabilities.
    pub capabilities: FfiPaykitReceiverCapabilities,
    /// Receiver-scoped public key used for Encrypted Links.
    pub noise_public_key: String,
}

/// Platform-owned, mode-specific payment adapter callbacks.
///
/// Public callbacks never receive private values, and private callbacks never
/// receive public values.
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
        counterparty_receiver_path: String,
    ) -> Result<Vec<FfiPrivateReceivingDetail>, PaykitFfiError>;

    /// Reserve receiving details for a counterparty's Private Payment List.
    fn reserve_private_receiving_details(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
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
        counterparty_receiver_path: &paykit_sdk::PaykitReceiverPath,
    ) -> paykit_sdk::Result<Vec<PrivateReceivingDetail>> {
        self.adapter
            .current_private_receiving_details(
                app_public_key(counterparty),
                counterparty_receiver_path.to_string(),
            )
            .map_err(|err| payment_adapter_error(err, "load private receiving details"))?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn reserve_private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &paykit_sdk::PaykitReceiverPath,
    ) -> paykit_sdk::Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        self.adapter
            .reserve_private_receiving_details(
                app_public_key(counterparty),
                counterparty_receiver_path.to_string(),
            )
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
            counterparty_receiver_path: request.counterparty_receiver_path.to_string(),
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
            counterparty_receiver_path: request.counterparty_receiver_path.to_string(),
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
        _counterparty_receiver_path: String,
    ) -> Result<Vec<FfiPrivateReceivingDetail>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn reserve_private_receiving_details(
        &self,
        _counterparty: String,
        _counterparty_receiver_path: String,
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
    /// List public Paykit receiver paths for a Pubky identity.
    pub async fn paykit_receiver_paths(
        &self,
        public_key: String,
    ) -> Result<Vec<String>, PaykitFfiError> {
        self.runtime
            .paykit_receiver_paths(parse_public_key(public_key)?)
            .await
            .map(|ids| ids.into_iter().map(|id| id.to_string()).collect())
            .map_err(Into::into)
    }

    /// Fetch one public Paykit receiver marker, if present.
    pub async fn paykit_receiver_marker(
        &self,
        public_key: String,
        receiver_path: String,
    ) -> Result<Option<FfiPaykitReceiverMarker>, PaykitFfiError> {
        self.runtime
            .paykit_receiver_marker(
                parse_public_key(public_key)?,
                paykit_sdk::PaykitReceiverPath::new(receiver_path)
                    .map_err(|err| validation_error(err.to_string()))?,
            )
            .await
            .map(|marker| marker.map(Into::into))
            .map_err(Into::into)
    }

    /// Publish the configured local receiver marker.
    pub async fn publish_paykit_receiver_marker(
        &self,
        capabilities: FfiPaykitReceiverCapabilities,
    ) -> Result<FfiPaykitReceiverMarker, PaykitFfiError> {
        self.runtime
            .publish_paykit_receiver_marker(capabilities.into())
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Remove the configured local receiver marker.
    pub async fn remove_paykit_receiver_marker(&self) -> Result<(), PaykitFfiError> {
        self.runtime
            .remove_paykit_receiver_marker()
            .await
            .map_err(Into::into)
    }

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

impl From<FfiPaykitReceiverCapabilities> for PaykitReceiverCapabilities {
    fn from(value: FfiPaykitReceiverCapabilities) -> Self {
        Self {
            private_payments: value.private_payments,
            payment_requests: value.payment_requests,
            receipts: value.receipts,
            outgoing_payments: value.outgoing_payments,
        }
    }
}

impl From<PaykitReceiverCapabilities> for FfiPaykitReceiverCapabilities {
    fn from(value: PaykitReceiverCapabilities) -> Self {
        Self {
            private_payments: value.private_payments,
            payment_requests: value.payment_requests,
            receipts: value.receipts,
            outgoing_payments: value.outgoing_payments,
        }
    }
}

impl From<PaykitReceiverMarker> for FfiPaykitReceiverMarker {
    fn from(value: PaykitReceiverMarker) -> Self {
        Self {
            receiver_path: value.receiver_path.to_string(),
            capabilities: value.capabilities.into(),
            noise_public_key: value.noise_public_key.z32(),
        }
    }
}

impl TryFrom<FfiPublicReceivingDetail> for PublicReceivingDetail {
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiPublicReceivingDetail) -> Result<Self, Self::Error> {
        Ok(Self {
            identifier: value.identifier,
            payload: value.payload.export_text(),
        })
    }
}

impl TryFrom<FfiPrivateReceivingDetail> for PrivateReceivingDetail {
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiPrivateReceivingDetail) -> Result<Self, Self::Error> {
        Ok(Self {
            identifier: value.identifier,
            payload: value.payload.export_text(),
        })
    }
}

impl TryFrom<FfiPrivatePaymentEndpointReservation> for PrivatePaymentEndpointReservation {
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiPrivatePaymentEndpointReservation) -> Result<Self, Self::Error> {
        let receiving_detail: PrivateReceivingDetail = value.receiving_detail.try_into()?;
        payment_endpoint_reservation_from_parts(
            value.reservation_id,
            receiving_detail.identifier,
            receiving_detail.payload,
            value.expires_at,
            value.attribution.export_fields(),
        )
    }
}

pub(crate) fn payment_endpoint_reservation_from_parts(
    reservation_id: String,
    identifier: String,
    payload: String,
    expires_at: Option<String>,
    attribution: HashMap<String, String>,
) -> paykit_sdk::Result<PrivatePaymentEndpointReservation> {
    Ok(PrivatePaymentEndpointReservation {
        reservation_id,
        receiving_detail: PrivateReceivingDetail {
            identifier,
            payload,
        },
        expires_at: expires_at.map(parse_rfc3339_utc).transpose()?,
        attribution,
    })
}

impl TryFrom<FfiPrivateReceivingDetailReservationResponse>
    for Option<Vec<PrivatePaymentEndpointReservation>>
{
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiPrivateReceivingDetailReservationResponse) -> Result<Self, Self::Error> {
        match value.kind {
            FfiPrivateReceivingDetailReservationResponseKind::UseCurrentReceivingDetails => {
                if !value.reservations.is_empty() {
                    return Err(paykit_sdk::PaykitSdkError::PaymentAdapter {
                        context:
                            "reservation response cannot include reservations when using current details"
                                .into(),
                        source: None,
                    });
                }
                Ok(None)
            }
            FfiPrivateReceivingDetailReservationResponseKind::Reservations => value
                .reservations
                .into_iter()
                .map(TryInto::try_into)
                .collect::<paykit_sdk::Result<Vec<_>>>()
                .map(Some),
            FfiPrivateReceivingDetailReservationResponseKind::Unknown => {
                Err(paykit_sdk::PaykitSdkError::PaymentAdapter {
                    context: "unknown receiving-detail reservation response kind".into(),
                    source: None,
                })
            }
        }
    }
}

impl From<PrivatePaymentEndpointReservationCancellation>
    for FfiPrivatePaymentEndpointReservationCancellation
{
    fn from(value: PrivatePaymentEndpointReservationCancellation) -> Self {
        Self {
            reservation_id: value.reservation_id,
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_path: value.counterparty_receiver_path.to_string(),
            identifier: value.identifier,
            payload_hash: value.payload_hash,
            attribution: Arc::new(FfiReservationAttribution::new(value.attribution)),
        }
    }
}

impl From<PaymentAmountContext> for FfiPaymentAmountContext {
    fn from(value: PaymentAmountContext) -> Self {
        Self {
            value: value.value,
            asset: value.asset,
        }
    }
}

impl FfiPublicPaymentEndpointCandidate {
    fn from_candidate(value: &PublicPaymentEndpointCandidate, candidate_id: String) -> Self {
        Self {
            candidate_id,
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_path: value.counterparty_receiver_path.to_string(),
            identifier: value.identifier.clone(),
            payload: Arc::new(FfiPaymentPayload::new(value.payload.clone())),
        }
    }
}

impl FfiPrivatePaymentEndpointCandidate {
    fn from_candidate(value: &PrivatePaymentEndpointCandidate, candidate_id: String) -> Self {
        Self {
            candidate_id,
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_path: value.counterparty_receiver_path.to_string(),
            identifier: value.identifier.clone(),
            payload: Arc::new(FfiPaymentPayload::new(value.payload.clone())),
        }
    }
}

impl From<FfiPaymentTarget> for PaymentTarget {
    fn from(value: FfiPaymentTarget) -> Self {
        Self {
            payload: value.payload.export_text(),
        }
    }
}

impl From<EndpointSyncChange> for FfiEndpointSyncChange {
    fn from(value: EndpointSyncChange) -> Self {
        Self {
            identifier: value.identifier,
            status: value.status.into(),
            error: value.error,
        }
    }
}

impl From<EndpointSyncReport> for FfiEndpointSyncReport {
    fn from(value: EndpointSyncReport) -> Self {
        Self {
            published: value.published.into_iter().map(Into::into).collect(),
            removed: value.removed.into_iter().map(Into::into).collect(),
            failed: value.failed.into_iter().map(Into::into).collect(),
        }
    }
}

fn public_candidate_id(candidate: &PublicPaymentEndpointCandidate) -> String {
    candidate_id(
        &candidate.counterparty,
        &candidate.counterparty_receiver_path,
        "public",
        &candidate.identifier,
        &candidate.payload,
    )
}

fn private_candidate_id(candidate: &PrivatePaymentEndpointCandidate) -> String {
    candidate_id(
        &candidate.counterparty,
        &candidate.counterparty_receiver_path,
        "private",
        &candidate.identifier,
        &candidate.payload,
    )
}

fn candidate_id(
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &paykit_sdk::PaykitReceiverPath,
    source: &str,
    identifier: &str,
    payload: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(counterparty.as_str().as_bytes());
    digest.update([0]);
    digest.update(counterparty_receiver_path.as_str().as_bytes());
    digest.update([0]);
    digest.update(source.as_bytes());
    digest.update([0]);
    digest.update(identifier.as_bytes());
    digest.update([0]);
    digest.update(payload.as_bytes());
    let digest = digest.finalize();
    format!("candidate-{}", hex::encode(&digest[..16]))
}

fn selected_candidates<T, F>(
    selected_ids: Vec<String>,
    candidates_by_id: &[(String, F)],
    candidates: &[T],
    context: &'static str,
) -> paykit_sdk::Result<Vec<T>>
where
    T: Clone,
{
    let mut selected = Vec::with_capacity(selected_ids.len());
    let mut seen = HashSet::new();
    for selected_id in selected_ids {
        if !seen.insert(selected_id.clone()) {
            return Err(payment_adapter_error(
                validation_error(format!("duplicate candidate id '{selected_id}'")),
                context,
            ));
        }
        let Some((index, _)) = candidates_by_id
            .iter()
            .enumerate()
            .find(|(_, (candidate_id, _))| candidate_id == &selected_id)
        else {
            return Err(payment_adapter_error(
                validation_error(format!("unknown candidate id '{selected_id}'")),
                context,
            ));
        };
        selected.push(candidates[index].clone());
    }
    Ok(selected)
}

pub(crate) fn parse_rfc3339_utc(value: String) -> paykit_sdk::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|err| paykit_sdk::PaykitSdkError::Protocol {
            context: format!("invalid RFC3339 time: {err}"),
            source: None,
        })
}

fn payment_adapter_error(err: PaykitFfiError, context: &'static str) -> paykit_sdk::PaykitSdkError {
    paykit_sdk::PaykitSdkError::PaymentAdapter {
        context: context.into(),
        source: Some(anyhow::Error::new(err)),
    }
}

fn payment_adapter_unavailable() -> PaykitFfiError {
    PaykitFfiError::PaymentAdapter {
        code: "payment_adapter_unavailable".into(),
        context: "payment adapter callbacks are not available on this SDK handle".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestPaymentAdapter {
        selected_ids: Vec<String>,
        built_candidate_ids: Arc<Mutex<Vec<String>>>,
    }

    impl FfiSdkPaymentAdapter for TestPaymentAdapter {
        fn current_public_receiving_details(
            &self,
        ) -> Result<Vec<FfiPublicReceivingDetail>, PaykitFfiError> {
            Ok(vec![FfiPublicReceivingDetail {
                identifier: "btc-mainnet-address".into(),
                payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
            }])
        }

        fn current_private_receiving_details(
            &self,
            _counterparty: String,
            _counterparty_receiver_path: String,
        ) -> Result<Vec<FfiPrivateReceivingDetail>, PaykitFfiError> {
            Ok(vec![FfiPrivateReceivingDetail {
                identifier: "btc-mainnet-address".into(),
                payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
            }])
        }

        fn reserve_private_receiving_details(
            &self,
            _counterparty: String,
            _counterparty_receiver_path: String,
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
            Ok(())
        }

        fn select_public_payment_endpoint_ids(
            &self,
            request: FfiPublicPaymentEndpointSelectionRequest,
        ) -> Result<Vec<String>, PaykitFfiError> {
            assert!(request.candidates[0].candidate_id.starts_with("candidate-"));
            Ok(self.selected_ids.clone())
        }

        fn build_public_payment_target(
            &self,
            endpoint: FfiPublicPaymentEndpointCandidate,
        ) -> Result<FfiPaymentTarget, PaykitFfiError> {
            self.built_candidate_ids
                .lock()
                .unwrap()
                .push(endpoint.candidate_id.clone());
            Ok(FfiPaymentTarget {
                payload: Arc::new(FfiPaymentPayload::new(format!(
                    "target:{}",
                    endpoint.identifier
                ))),
            })
        }

        fn select_private_payment_endpoint_ids(
            &self,
            request: FfiPrivatePaymentEndpointSelectionRequest,
        ) -> Result<Vec<String>, PaykitFfiError> {
            assert!(request.candidates[0].candidate_id.starts_with("candidate-"));
            Ok(self.selected_ids.clone())
        }

        fn build_private_payment_target(
            &self,
            endpoint: FfiPrivatePaymentEndpointCandidate,
        ) -> Result<FfiPaymentTarget, PaykitFfiError> {
            self.built_candidate_ids
                .lock()
                .unwrap()
                .push(endpoint.candidate_id.clone());
            Ok(FfiPaymentTarget {
                payload: Arc::new(FfiPaymentPayload::new(format!(
                    "target:{}",
                    endpoint.identifier
                ))),
            })
        }
    }

    fn candidate(identifier: &str) -> PublicPaymentEndpointCandidate {
        PublicPaymentEndpointCandidate {
            counterparty: PubkyPublicKey::new(
                "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
            )
            .unwrap(),
            counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet")
                .unwrap(),
            identifier: identifier.into(),
            payload: format!("payload:{identifier}"),
        }
    }

    fn private_candidate(identifier: &str) -> PrivatePaymentEndpointCandidate {
        PrivatePaymentEndpointCandidate {
            counterparty: PubkyPublicKey::new(
                "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
            )
            .unwrap(),
            counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet")
                .unwrap(),
            identifier: identifier.into(),
            payload: format!("payload:{identifier}"),
        }
    }

    #[tokio::test]
    async fn test_select_public_payment_endpoint_ids_maps_back_to_candidates() {
        let candidates = vec![
            candidate("btc-mainnet-address"),
            candidate("btc-mainnet-lnurl"),
        ];
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter {
                selected_ids: vec![public_candidate_id(&candidates[1])],
                built_candidate_ids: Arc::default(),
            }),
        };
        let selected = adapter
            .select_public_payment_endpoints(&PublicPaymentEndpointSelectionRequest {
                counterparty: candidates[0].counterparty.clone(),
                counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet")
                    .unwrap(),
                amount: Some(PaymentAmountContext {
                    value: "1.00".into(),
                    asset: "btc".into(),
                }),
                candidates: candidates.clone(),
            })
            .await
            .unwrap();

        assert_eq!(selected, vec![candidates[1].clone()]);
    }

    #[tokio::test]
    async fn test_select_public_payment_endpoint_ids_rejects_unknown_ids() {
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter {
                selected_ids: vec!["missing".into()],
                built_candidate_ids: Arc::default(),
            }),
        };
        let candidates = vec![candidate("btc-mainnet-address")];
        let err = adapter
            .select_public_payment_endpoints(&PublicPaymentEndpointSelectionRequest {
                counterparty: candidates[0].counterparty.clone(),
                counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet")
                    .unwrap(),
                amount: None,
                candidates,
            })
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            paykit_sdk::PaykitSdkError::PaymentAdapter { .. }
        ));
    }

    #[tokio::test]
    async fn test_select_private_payment_endpoint_ids_maps_back_to_candidates() {
        let candidates = vec![
            private_candidate("btc-mainnet-address"),
            private_candidate("btc-mainnet-lnurl"),
        ];
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter {
                selected_ids: vec![private_candidate_id(&candidates[1])],
                built_candidate_ids: Arc::default(),
            }),
        };

        let selected = adapter
            .select_private_payment_endpoints(&PrivatePaymentEndpointSelectionRequest {
                counterparty: candidates[0].counterparty.clone(),
                counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet")
                    .unwrap(),
                amount: None,
                candidates: candidates.clone(),
            })
            .await
            .unwrap();

        assert_eq!(selected, vec![candidates[1].clone()]);
    }

    #[test]
    fn test_public_and_private_candidates_have_distinct_ids() {
        let public = candidate("btc-mainnet-address");
        let private = private_candidate("btc-mainnet-address");

        assert_ne!(public_candidate_id(&public), private_candidate_id(&private));
    }

    #[tokio::test]
    async fn test_build_public_payment_target_maps_payload() {
        let built_candidate_ids = Arc::new(Mutex::new(Vec::new()));
        let endpoint = candidate("btc-mainnet-address");
        let expected_id = public_candidate_id(&endpoint);
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter {
                selected_ids: Vec::new(),
                built_candidate_ids: built_candidate_ids.clone(),
            }),
        };
        let target = adapter
            .build_public_payment_target(&endpoint)
            .await
            .unwrap();

        assert_eq!(target.payload, "target:btc-mainnet-address");
        assert_eq!(*built_candidate_ids.lock().unwrap(), vec![expected_id]);
    }

    #[test]
    fn test_reservation_response_rejects_mixed_meaning() {
        let response = FfiPrivateReceivingDetailReservationResponse {
            kind: FfiPrivateReceivingDetailReservationResponseKind::UseCurrentReceivingDetails,
            reservations: vec![FfiPrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: FfiPrivateReceivingDetail {
                    identifier: "btc-mainnet-address".into(),
                    payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
                },
                expires_at: None,
                attribution: Arc::new(FfiReservationAttribution::new(HashMap::new())),
            }],
        };

        let result: paykit_sdk::Result<Option<Vec<PrivatePaymentEndpointReservation>>> =
            response.try_into();

        assert!(matches!(
            result,
            Err(paykit_sdk::PaykitSdkError::PaymentAdapter { .. })
        ));
    }

    #[test]
    fn test_payment_endpoint_reservation_parses_expiry() {
        let reservation = FfiPrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: FfiPrivateReceivingDetail {
                identifier: "btc-mainnet-address".into(),
                payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
            },
            expires_at: Some("2026-06-18T11:00:00Z".into()),
            attribution: Arc::new(FfiReservationAttribution::new(HashMap::new())),
        };
        let reservation = PrivatePaymentEndpointReservation::try_from(reservation).unwrap();

        assert_eq!(
            reservation.expires_at.unwrap().to_rfc3339(),
            "2026-06-18T11:00:00+00:00"
        );
    }

    #[test]
    fn test_noop_adapter_reports_unavailable_for_receiving_details() {
        let err = FfiNoopSdkPaymentAdapter
            .current_public_receiving_details()
            .unwrap_err();

        assert!(matches!(
            err,
            PaykitFfiError::PaymentAdapter { code, .. }
                if code == "payment_adapter_unavailable"
        ));
    }
}
