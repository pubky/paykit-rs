use super::*;

pub(in crate::runtime) fn private_candidate_batch(
    counterparty: &PubkyPublicKey,
    views: &[PrivatePaymentListView],
    after_private_payment_list_version: Option<u64>,
) -> Result<Option<PrivatePaymentCandidateBatch>> {
    if views.is_empty() {
        return Ok(None);
    }
    let mut private_payment_list_version = 0;
    let mut candidates = Vec::new();
    for view in views {
        let stream_item_id =
            view.latest_stream_item_id
                .ok_or_else(|| PaykitSdkError::Protocol {
                    context: "current Private Payment List has no stream item id".into(),
                    source: None,
                })?;
        private_payment_list_version = private_payment_list_version.max(stream_item_id);
        if after_private_payment_list_version.is_none_or(|version| stream_item_id > version) {
            candidates.extend(view.payment_endpoints.iter().map(|(identifier, payload)| {
                PrivatePaymentEndpointCandidate {
                    counterparty: counterparty.clone(),
                    app_id: view.app_id.clone(),
                    identifier: identifier.clone(),
                    payload: payload.clone(),
                }
            }));
        }
    }
    candidates.sort_by(|left, right| {
        left.app_id
            .as_str()
            .cmp(right.app_id.as_str())
            .then_with(|| left.identifier.cmp(&right.identifier))
    });
    Ok(Some(PrivatePaymentCandidateBatch {
        private_payment_list_version,
        candidates,
    }))
}

pub(in crate::runtime) struct PrivatePaymentCandidateBatch {
    pub(super) private_payment_list_version: u64,
    candidates: Vec<PrivatePaymentEndpointCandidate>,
}

pub(super) fn payment_request_amount(terms: &PaymentRequestTermsRecord) -> PaymentAmountContext {
    PaymentAmountContext {
        value: terms.amount.value.clone(),
        asset: terms.amount.asset.clone(),
    }
}

fn payment_request_allows_endpoint(
    terms: &PaymentRequestTermsRecord,
    app_id: &paykit_lib::PaykitAppId,
    identifier: &str,
) -> bool {
    terms
        .required_app_id
        .as_ref()
        .is_none_or(|required_app_id| required_app_id == app_id)
        && terms
            .accepted_payment_endpoint_identifiers
            .iter()
            .any(|accepted| accepted == identifier)
}

pub(in crate::runtime) fn filter_private_candidate_batch_for_request(
    candidate_batch: Option<&mut PrivatePaymentCandidateBatch>,
    terms: Option<&PaymentRequestTermsRecord>,
) {
    let (Some(candidate_batch), Some(terms)) = (candidate_batch, terms) else {
        return;
    };
    candidate_batch.candidates.retain(|candidate| {
        payment_request_allows_endpoint(terms, &candidate.app_id, &candidate.identifier)
    });
}

pub(in crate::runtime) fn filter_public_candidates_for_request(
    candidates: &mut Vec<PublicPaymentEndpointCandidate>,
    terms: Option<&PaymentRequestTermsRecord>,
) {
    let Some(terms) = terms else {
        return;
    };
    candidates.retain(|candidate| {
        payment_request_allows_endpoint(terms, &candidate.app_id, &candidate.identifier)
    });
}

impl PrivatePaymentCandidateBatch {
    pub(in crate::runtime) fn has_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub(in crate::runtime) fn is_newer_than(&self, previous_version: Option<u64>) -> bool {
        previous_version.is_none_or(|version| self.private_payment_list_version > version)
    }

    pub(in crate::runtime) fn candidates(&self) -> Vec<PrivatePaymentEndpointCandidate> {
        self.candidates.clone()
    }

    pub(super) fn sort_by_app_preferences(&mut self, registry: &paykit_lib::PaykitAppRegistry) {
        self.candidates.sort_by(|left, right| {
            let left_identifier = PaymentEndpointIdentifier::new(&left.identifier).ok();
            let right_identifier = PaymentEndpointIdentifier::new(&right.identifier).ok();
            let left_rank = left_identifier
                .as_ref()
                .map(|identifier| app_preference_rank(registry, &left.app_id, identifier))
                .unwrap_or(2);
            let right_rank = right_identifier
                .as_ref()
                .map(|identifier| app_preference_rank(registry, &right.app_id, identifier))
                .unwrap_or(2);
            left_rank.cmp(&right_rank).then_with(|| {
                left.app_id
                    .as_str()
                    .cmp(right.app_id.as_str())
                    .then_with(|| left.identifier.cmp(&right.identifier))
            })
        });
    }
}

pub(in crate::runtime) fn filter_private_views_by_authorized_apps(
    views: &mut Vec<PrivatePaymentListView>,
    authorized_app_ids: Option<&[paykit_lib::PaykitAppId]>,
) {
    let Some(authorized_app_ids) = authorized_app_ids else {
        views.clear();
        return;
    };
    views.retain(|view| authorized_app_ids.contains(&view.app_id));
}

pub(in crate::runtime) fn app_preference_rank(
    registry: &paykit_lib::PaykitAppRegistry,
    app_id: &paykit_lib::PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
) -> u8 {
    if registry.default_apps_by_endpoint().get(identifier) == Some(app_id) {
        0
    } else if registry.default_app_id() == Some(app_id) {
        1
    } else {
        2
    }
}

pub(in crate::runtime) fn public_app_load_order(
    registry: &paykit_lib::PaykitAppRegistry,
    required_app_id: Option<&paykit_lib::PaykitAppId>,
) -> Vec<paykit_lib::PaykitAppId> {
    let endpoint_defaults = registry
        .default_apps_by_endpoint()
        .values()
        .collect::<Vec<_>>();
    let mut app_ids = registry.apps().keys().cloned().collect::<Vec<_>>();
    app_ids.sort_by(|left, right| {
        let rank = |app_id: &paykit_lib::PaykitAppId| {
            if required_app_id == Some(app_id) {
                0
            } else if endpoint_defaults.contains(&app_id) {
                1
            } else if registry.default_app_id() == Some(app_id) {
                2
            } else {
                3
            }
        };
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.as_str().cmp(right.as_str()))
    });
    app_ids
}

pub(in crate::runtime) fn unresolved_public_resolution(
    had_candidates: bool,
    failures: Vec<PublicPaymentEndpointLoadFailure>,
    loaded_app_count: usize,
) -> PublicContactPaymentResolution {
    PublicContactPaymentResolution {
        status: if loaded_app_count == 0 && !failures.is_empty() {
            PublicPaymentResolutionStatus::Unavailable
        } else if had_candidates {
            PublicPaymentResolutionStatus::UnsupportedEndpoint
        } else {
            PublicPaymentResolutionStatus::NoEndpoint
        },
        payable_endpoints: Vec::new(),
        failures,
    }
}

pub(super) fn unresolved_private_resolution(
    had_candidates: bool,
    state: PrivatePaymentResolutionState,
    private_payment_list_version: Option<u64>,
) -> PrivateContactPaymentResolution {
    PrivateContactPaymentResolution {
        status: if had_candidates {
            PrivatePaymentResolutionStatus::UnsupportedEndpoint
        } else {
            PrivatePaymentResolutionStatus::NoEndpoint
        },
        state,
        private_payment_list_version,
        payable_endpoints: Vec::new(),
    }
}

pub(super) fn waiting_for_updated_private_payment_list(
    state: PrivatePaymentResolutionState,
    private_payment_list_version: u64,
) -> PrivateContactPaymentResolution {
    PrivateContactPaymentResolution {
        status: PrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList,
        state,
        private_payment_list_version: Some(private_payment_list_version),
        payable_endpoints: Vec::new(),
    }
}

pub(in crate::runtime) fn public_payable_from_batch(
    selected: &[PublicPaymentEndpointCandidate],
    candidates: &[PublicPaymentEndpointCandidate],
) -> Result<Vec<PublicPaymentEndpointCandidate>> {
    payable_from_batch(selected, candidates)
}

pub(in crate::runtime) fn private_payable_from_batch(
    selected: &[PrivatePaymentEndpointCandidate],
    candidates: &[PrivatePaymentEndpointCandidate],
) -> Result<Vec<PrivatePaymentEndpointCandidate>> {
    payable_from_batch(selected, candidates)
}

fn payable_from_batch<T>(selected: &[T], candidates: &[T]) -> Result<Vec<T>>
where
    T: Clone + PartialEq,
{
    let mut payable = Vec::with_capacity(selected.len());
    for candidate in selected {
        if !candidates.contains(candidate) {
            return Err(PaykitSdkError::Protocol {
                context:
                    "PaymentAdapter returned a payable endpoint that was not in the candidate batch"
                        .into(),
                source: None,
            });
        }
        if payable.contains(candidate) {
            return Err(PaykitSdkError::Protocol {
                context: "PaymentAdapter returned duplicate payable endpoints".into(),
                source: None,
            });
        }
        payable.push(candidate.clone());
    }
    Ok(payable)
}
