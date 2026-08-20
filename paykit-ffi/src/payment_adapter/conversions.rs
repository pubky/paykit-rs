use std::{collections::HashSet, sync::Arc};

use chrono::{DateTime, Utc};
use paykit_sdk::{
    EndpointSyncChange, EndpointSyncReport, PaymentAmountContext, PaymentTarget,
    PrivatePaymentEndpointCandidate, PrivatePaymentEndpointReservation,
    PrivatePaymentEndpointReservationCancellation, PrivateReceivingDetail, PubkyPublicKey,
    PublicPaymentEndpointCandidate, PublicReceivingDetail,
};
use sha2::{Digest, Sha256};

use super::{
    FfiEndpointSyncChange, FfiEndpointSyncReport, FfiPaymentAmountContext, FfiPaymentPayload,
    FfiPaymentTarget, FfiPrivatePaymentEndpointCandidate, FfiPrivatePaymentEndpointReservation,
    FfiPrivatePaymentEndpointReservationCancellation, FfiPrivateReceivingDetail,
    FfiPrivateReceivingDetailReservationResponse, FfiPrivateReceivingDetailReservationResponseKind,
    FfiPublicPaymentEndpointCandidate, FfiPublicReceivingDetail, FfiReservationAttribution,
};
use crate::{
    errors::{validation_error, PaykitFfiError},
    session::app_public_key,
};

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
        .map_err(|err| paykit_sdk::PaykitSdkError::PaymentAdapter {
            context: "payment adapter returned an invalid private endpoint reservation".into(),
            source: Some(anyhow::Error::new(err)),
        })
    }
}

pub(crate) fn payment_endpoint_reservation_from_parts(
    reservation_id: String,
    identifier: String,
    payload: String,
    expires_at: Option<String>,
    attribution: std::collections::HashMap<String, String>,
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
    pub(super) fn from_candidate(
        value: &PublicPaymentEndpointCandidate,
        candidate_id: String,
    ) -> Self {
        Self {
            candidate_id,
            counterparty: app_public_key(&value.counterparty),
            app_id: value.app_id.to_string(),
            identifier: value.identifier.clone(),
            payload: Arc::new(FfiPaymentPayload::new(value.payload.clone())),
        }
    }
}

impl FfiPrivatePaymentEndpointCandidate {
    pub(super) fn from_candidate(
        value: &PrivatePaymentEndpointCandidate,
        candidate_id: String,
    ) -> Self {
        Self {
            candidate_id,
            counterparty: app_public_key(&value.counterparty),
            app_id: value.app_id.to_string(),
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

pub(super) fn public_candidate_id(candidate: &PublicPaymentEndpointCandidate) -> String {
    candidate_id(
        &candidate.counterparty,
        &candidate.app_id,
        "public",
        &candidate.identifier,
        &candidate.payload,
    )
}

pub(super) fn private_candidate_id(candidate: &PrivatePaymentEndpointCandidate) -> String {
    candidate_id(
        &candidate.counterparty,
        &candidate.app_id,
        "private",
        &candidate.identifier,
        &candidate.payload,
    )
}

fn candidate_id(
    counterparty: &PubkyPublicKey,
    app_id: &paykit_sdk::PaykitAppId,
    source: &str,
    identifier: &str,
    payload: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(counterparty.as_str().as_bytes());
    digest.update([0]);
    digest.update(app_id.as_str().as_bytes());
    digest.update([0]);
    digest.update(source.as_bytes());
    digest.update([0]);
    digest.update(identifier.as_bytes());
    digest.update([0]);
    digest.update(payload.as_bytes());
    let digest = digest.finalize();
    format!("candidate-{}", hex::encode(&digest[..16]))
}

pub(super) fn selected_candidates<T, F>(
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

pub(super) fn payment_adapter_error(
    err: PaykitFfiError,
    context: &'static str,
) -> paykit_sdk::PaykitSdkError {
    paykit_sdk::PaykitSdkError::PaymentAdapter {
        context: context.into(),
        source: Some(anyhow::Error::new(err)),
    }
}

pub(super) fn payment_adapter_unavailable() -> PaykitFfiError {
    PaykitFfiError::PaymentAdapter {
        code: "payment_adapter_unavailable".into(),
        context: "payment adapter callbacks are not available on this SDK handle".into(),
    }
}
