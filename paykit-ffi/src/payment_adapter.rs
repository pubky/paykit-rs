use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use paykit_sdk::{
    EndpointSyncChange, EndpointSyncReport, PaymentAdapter, PaymentAmountContext,
    PaymentEndpointCandidate, PaymentEndpointReservation, PaymentEndpointReservationCancellation,
    PaymentEndpointSelectionRequest, PaymentEndpointSource, PaymentTarget, PubkyPublicKey,
    ReceivingDetail, ReceivingDetailScope,
};

use crate::{
    errors::{validation_error, PaykitFfiError},
    profiles::FfiPublicationStatus,
    sdk::FfiPaykitSdk,
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

/// Scope used when asking a payment adapter for receiving details.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiReceivingDetailScopeKind {
    /// Details intended for public Payment Endpoints.
    Public,
    /// Details intended for one counterparty's Private Payment List.
    Private,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Receiving-detail request scope passed to the payment adapter.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiReceivingDetailScope {
    /// Scope kind.
    pub kind: FfiReceivingDetailScopeKind,
    /// Counterparty public key for private scopes.
    pub counterparty: Option<String>,
}

/// Payment-method-specific receiving detail returned by the payment adapter.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Receiving detail reserved by the payment adapter.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentEndpointReservation {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Reserved receiving detail.
    pub receiving_detail: FfiReceivingDetail,
    /// Optional reservation expiry as RFC3339 text.
    pub expires_at: Option<String>,
    /// Adapter attribution metadata.
    pub attribution: Arc<FfiReservationAttribution>,
}

/// Request passed to cancel a receiving-detail reservation.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentEndpointReservationCancellation {
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

/// Source of a discovered Payment Endpoint candidate.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPaymentEndpointSource {
    /// Endpoint came from a counterparty-specific Private Payment List.
    PrivatePaymentList,
    /// Endpoint came from a public Payment Endpoint.
    PublicPaymentEndpoint,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Optional amount context for endpoint selection.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaymentAmountContext {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Candidate endpoint passed to the payment adapter.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentEndpointCandidate {
    /// Opaque candidate id for this callback request.
    pub candidate_id: String,
    /// Counterparty that published the endpoint.
    pub counterparty: String,
    /// Where the endpoint was discovered.
    pub source: FfiPaymentEndpointSource,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Request passed to the payment adapter for payable endpoint ordering.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: String,
    /// Optional amount context.
    pub amount: Option<FfiPaymentAmountContext>,
    /// Candidate endpoints in SDK preference order.
    pub candidates: Vec<FfiPaymentEndpointCandidate>,
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

/// Platform-owned payment adapter callbacks.
#[uniffi::export(with_foreign)]
pub trait FfiSdkPaymentAdapter: Send + Sync {
    /// Return current receiving details for a scope.
    fn current_receiving_details(
        &self,
        scope: FfiReceivingDetailScope,
    ) -> Result<Vec<FfiReceivingDetail>, PaykitFfiError>;

    /// Reserve receiving details for a counterparty's Private Payment List.
    fn reserve_receiving_details(
        &self,
        counterparty: String,
    ) -> Result<Option<Vec<FfiPaymentEndpointReservation>>, PaykitFfiError>;

    /// Cancel a previously reserved receiving detail.
    fn cancel_receiving_detail_reservation(
        &self,
        cancellation: FfiPaymentEndpointReservationCancellation,
    ) -> Result<(), PaykitFfiError>;

    /// Return payable candidate ids in adapter-preferred order.
    fn select_payment_endpoint_ids(
        &self,
        request: FfiPaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError>;

    /// Build a payment target from a payable endpoint.
    fn build_payment_target(
        &self,
        endpoint: FfiPaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError>;
}

#[derive(Clone)]
pub(crate) struct FfiSdkPaymentAdapterAdapter {
    pub(crate) adapter: Arc<dyn FfiSdkPaymentAdapter>,
}

#[async_trait]
impl PaymentAdapter for FfiSdkPaymentAdapterAdapter {
    async fn current_receiving_details(
        &self,
        scope: ReceivingDetailScope,
    ) -> paykit_sdk::Result<Vec<ReceivingDetail>> {
        self.adapter
            .current_receiving_details(scope.into())
            .map_err(|err| payment_adapter_error(err, "load current receiving details"))?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn reserve_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> paykit_sdk::Result<Option<Vec<PaymentEndpointReservation>>> {
        self.adapter
            .reserve_receiving_details(counterparty.to_string())
            .map_err(|err| payment_adapter_error(err, "reserve receiving details"))?
            .map(|reservations| reservations.into_iter().map(TryInto::try_into).collect())
            .transpose()
    }

    async fn cancel_receiving_detail_reservation(
        &self,
        cancellation: &PaymentEndpointReservationCancellation,
    ) -> paykit_sdk::Result<()> {
        self.adapter
            .cancel_receiving_detail_reservation(cancellation.clone().into())
            .map_err(|err| payment_adapter_error(err, "cancel receiving detail reservation"))
    }

    async fn select_payment_endpoints(
        &self,
        request: &PaymentEndpointSelectionRequest,
    ) -> paykit_sdk::Result<Vec<PaymentEndpointCandidate>> {
        let candidates_by_id = request
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                (
                    candidate_id(index),
                    FfiPaymentEndpointCandidate::from_candidate(candidate, candidate_id(index)),
                )
            })
            .collect::<Vec<_>>();
        let ffi_request = FfiPaymentEndpointSelectionRequest {
            counterparty: request.counterparty.to_string(),
            amount: request.amount.clone().map(Into::into),
            candidates: candidates_by_id
                .iter()
                .map(|(_, candidate)| candidate.clone())
                .collect(),
        };
        let selected_ids = self
            .adapter
            .select_payment_endpoint_ids(ffi_request)
            .map_err(|err| payment_adapter_error(err, "select payment endpoints"))?;

        let mut selected = Vec::with_capacity(selected_ids.len());
        let mut seen = HashSet::new();
        for selected_id in selected_ids {
            if !seen.insert(selected_id.clone()) {
                return Err(payment_adapter_error(
                    validation_error(format!("duplicate candidate id '{selected_id}'")),
                    "select payment endpoints",
                ));
            }
            let Some((index, _)) = candidates_by_id
                .iter()
                .enumerate()
                .find(|(_, (candidate_id, _))| candidate_id == &selected_id)
            else {
                return Err(payment_adapter_error(
                    validation_error(format!("unknown candidate id '{selected_id}'")),
                    "select payment endpoints",
                ));
            };
            selected.push(request.candidates[index].clone());
        }

        Ok(selected)
    }

    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> paykit_sdk::Result<PaymentTarget> {
        self.adapter
            .build_payment_target(FfiPaymentEndpointCandidate::from_candidate(
                endpoint,
                "candidate".into(),
            ))
            .map_err(|err| payment_adapter_error(err, "build payment target"))
            .map(Into::into)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FfiNoopSdkPaymentAdapter;

impl FfiSdkPaymentAdapter for FfiNoopSdkPaymentAdapter {
    fn current_receiving_details(
        &self,
        _scope: FfiReceivingDetailScope,
    ) -> Result<Vec<FfiReceivingDetail>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn reserve_receiving_details(
        &self,
        _counterparty: String,
    ) -> Result<Option<Vec<FfiPaymentEndpointReservation>>, PaykitFfiError> {
        Ok(None)
    }

    fn cancel_receiving_detail_reservation(
        &self,
        _cancellation: FfiPaymentEndpointReservationCancellation,
    ) -> Result<(), PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn select_payment_endpoint_ids(
        &self,
        _request: FfiPaymentEndpointSelectionRequest,
    ) -> Result<Vec<String>, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }

    fn build_payment_target(
        &self,
        _endpoint: FfiPaymentEndpointCandidate,
    ) -> Result<FfiPaymentTarget, PaykitFfiError> {
        Err(payment_adapter_unavailable())
    }
}

#[uniffi::export]
impl FfiPaykitSdk {
    /// Publish current public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints(&self) -> Result<FfiEndpointSyncReport, PaykitFfiError> {
        self.runtime
            .sync_public_endpoints()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<ReceivingDetailScope> for FfiReceivingDetailScope {
    fn from(value: ReceivingDetailScope) -> Self {
        match value {
            ReceivingDetailScope::Public => Self {
                kind: FfiReceivingDetailScopeKind::Public,
                counterparty: None,
            },
            ReceivingDetailScope::Private { counterparty } => Self {
                kind: FfiReceivingDetailScopeKind::Private,
                counterparty: Some(counterparty.to_string()),
            },
            _ => Self {
                kind: FfiReceivingDetailScopeKind::Unknown,
                counterparty: None,
            },
        }
    }
}

impl TryFrom<FfiReceivingDetail> for ReceivingDetail {
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiReceivingDetail) -> Result<Self, Self::Error> {
        Ok(Self {
            identifier: value.identifier,
            payload: value.payload.export_text(),
        })
    }
}

impl TryFrom<FfiPaymentEndpointReservation> for PaymentEndpointReservation {
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiPaymentEndpointReservation) -> Result<Self, Self::Error> {
        Ok(Self {
            reservation_id: value.reservation_id,
            receiving_detail: value.receiving_detail.try_into()?,
            expires_at: value.expires_at.map(parse_rfc3339_utc).transpose()?,
            attribution: value.attribution.export_fields(),
        })
    }
}

impl From<PaymentEndpointReservationCancellation> for FfiPaymentEndpointReservationCancellation {
    fn from(value: PaymentEndpointReservationCancellation) -> Self {
        Self {
            reservation_id: value.reservation_id,
            counterparty: value.counterparty.to_string(),
            identifier: value.identifier,
            payload_hash: value.payload_hash,
            attribution: Arc::new(FfiReservationAttribution::new(value.attribution)),
        }
    }
}

impl From<PaymentEndpointSource> for FfiPaymentEndpointSource {
    fn from(value: PaymentEndpointSource) -> Self {
        match value {
            PaymentEndpointSource::PrivatePaymentList => Self::PrivatePaymentList,
            PaymentEndpointSource::PublicPaymentEndpoint => Self::PublicPaymentEndpoint,
            _ => Self::Unknown,
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

impl FfiPaymentEndpointCandidate {
    fn from_candidate(value: &PaymentEndpointCandidate, candidate_id: String) -> Self {
        Self {
            candidate_id,
            counterparty: value.counterparty.to_string(),
            source: value.source.clone().into(),
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

fn candidate_id(index: usize) -> String {
    format!("candidate-{index}")
}

fn parse_rfc3339_utc(value: String) -> paykit_sdk::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|err| paykit_sdk::PaykitSdkError::Protocol(format!("invalid RFC3339 time: {err}")))
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

    #[derive(Default)]
    struct TestPaymentAdapter {
        selected_ids: Vec<String>,
    }

    impl FfiSdkPaymentAdapter for TestPaymentAdapter {
        fn current_receiving_details(
            &self,
            _scope: FfiReceivingDetailScope,
        ) -> Result<Vec<FfiReceivingDetail>, PaykitFfiError> {
            Ok(vec![FfiReceivingDetail {
                identifier: "btc-mainnet-address".into(),
                payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
            }])
        }

        fn reserve_receiving_details(
            &self,
            _counterparty: String,
        ) -> Result<Option<Vec<FfiPaymentEndpointReservation>>, PaykitFfiError> {
            Ok(None)
        }

        fn cancel_receiving_detail_reservation(
            &self,
            _cancellation: FfiPaymentEndpointReservationCancellation,
        ) -> Result<(), PaykitFfiError> {
            Ok(())
        }

        fn select_payment_endpoint_ids(
            &self,
            request: FfiPaymentEndpointSelectionRequest,
        ) -> Result<Vec<String>, PaykitFfiError> {
            assert_eq!(request.candidates[0].candidate_id, "candidate-0");
            Ok(self.selected_ids.clone())
        }

        fn build_payment_target(
            &self,
            endpoint: FfiPaymentEndpointCandidate,
        ) -> Result<FfiPaymentTarget, PaykitFfiError> {
            Ok(FfiPaymentTarget {
                payload: Arc::new(FfiPaymentPayload::new(format!(
                    "target:{}",
                    endpoint.identifier
                ))),
            })
        }
    }

    fn candidate(identifier: &str) -> PaymentEndpointCandidate {
        PaymentEndpointCandidate {
            counterparty: PubkyPublicKey::new(
                "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
            )
            .unwrap(),
            source: PaymentEndpointSource::PublicPaymentEndpoint,
            identifier: identifier.into(),
            payload: format!("payload:{identifier}"),
        }
    }

    #[tokio::test]
    async fn test_select_payment_endpoint_ids_maps_back_to_candidates() {
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter {
                selected_ids: vec!["candidate-1".into()],
            }),
        };
        let candidates = vec![
            candidate("btc-mainnet-address"),
            candidate("btc-mainnet-lnurl"),
        ];
        let selected = adapter
            .select_payment_endpoints(&PaymentEndpointSelectionRequest {
                counterparty: candidates[0].counterparty.clone(),
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
    async fn test_select_payment_endpoint_ids_rejects_unknown_ids() {
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter {
                selected_ids: vec!["missing".into()],
            }),
        };
        let candidates = vec![candidate("btc-mainnet-address")];
        let err = adapter
            .select_payment_endpoints(&PaymentEndpointSelectionRequest {
                counterparty: candidates[0].counterparty.clone(),
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
    async fn test_build_payment_target_maps_payload() {
        let adapter = FfiSdkPaymentAdapterAdapter {
            adapter: Arc::new(TestPaymentAdapter::default()),
        };
        let target = adapter
            .build_payment_target(&candidate("btc-mainnet-address"))
            .await
            .unwrap();

        assert_eq!(target.payload, "target:btc-mainnet-address");
    }

    #[test]
    fn test_payment_endpoint_reservation_parses_expiry() {
        let reservation = FfiPaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: FfiReceivingDetail {
                identifier: "btc-mainnet-address".into(),
                payload: Arc::new(FfiPaymentPayload::new("bc1qexample".into())),
            },
            expires_at: Some("2026-06-18T11:00:00Z".into()),
            attribution: Arc::new(FfiReservationAttribution::new(HashMap::new())),
        };
        let reservation = PaymentEndpointReservation::try_from(reservation).unwrap();

        assert_eq!(
            reservation.expires_at.unwrap().to_rfc3339(),
            "2026-06-18T11:00:00+00:00"
        );
    }

    #[test]
    fn test_noop_adapter_reports_unavailable_for_receiving_details() {
        let err = FfiNoopSdkPaymentAdapter
            .current_receiving_details(FfiReceivingDetailScope {
                kind: FfiReceivingDetailScopeKind::Public,
                counterparty: None,
            })
            .unwrap_err();

        assert!(matches!(
            err,
            PaykitFfiError::PaymentAdapter { code, .. }
                if code == "payment_adapter_unavailable"
        ));
    }
}
