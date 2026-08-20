use super::*;
use crate::PaymentAmountContext;
use std::future::Future;

mod candidates;

pub(super) use candidates::{
    app_preference_rank, filter_private_candidate_batch_for_request,
    filter_private_views_by_authorized_apps, filter_public_candidates_for_request,
    private_candidate_batch, private_payable_from_batch, public_app_load_order,
    public_payable_from_batch, unresolved_public_resolution, PrivatePaymentCandidateBatch,
};
use candidates::{
    payment_request_amount, unresolved_private_resolution, waiting_for_updated_private_payment_list,
};

const PREPARE_PRIVATE_PAYMENT_SYNC_ROUND_LIMIT: usize = 8;
const PUBLIC_PAYMENT_RESOLUTION_MAX_ENDPOINTS: usize = 256;
const PUBLIC_PAYMENT_RESOLUTION_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct PublicPaymentCandidateLoad {
    candidates: Vec<PublicPaymentEndpointCandidate>,
    failures: Vec<PublicPaymentEndpointLoadFailure>,
    loaded_app_count: usize,
}

pub(in crate::runtime) struct PublicPaymentListLoad {
    pub(in crate::runtime) payment_lists: Vec<(paykit_lib::PaykitAppId, paykit_lib::PaymentList)>,
    pub(in crate::runtime) failures: Vec<PublicPaymentEndpointLoadFailure>,
    pub(in crate::runtime) loaded_app_count: usize,
}

struct PublicPaymentResolutionBudget {
    remaining_endpoints: usize,
    remaining_payload_bytes: usize,
}

impl Default for PublicPaymentResolutionBudget {
    fn default() -> Self {
        Self {
            remaining_endpoints: PUBLIC_PAYMENT_RESOLUTION_MAX_ENDPOINTS,
            remaining_payload_bytes: PUBLIC_PAYMENT_RESOLUTION_MAX_PAYLOAD_BYTES,
        }
    }
}

impl PublicPaymentResolutionBudget {
    fn remaining_limits(&self) -> Option<(usize, usize)> {
        (self.remaining_endpoints > 0 && self.remaining_payload_bytes > 0)
            .then_some((self.remaining_endpoints, self.remaining_payload_bytes))
    }

    fn consume(&mut self, endpoint_count: usize, payload_bytes: usize) -> bool {
        let current_endpoint_count =
            PUBLIC_PAYMENT_RESOLUTION_MAX_ENDPOINTS - self.remaining_endpoints;
        let current_payload_bytes =
            PUBLIC_PAYMENT_RESOLUTION_MAX_PAYLOAD_BYTES - self.remaining_payload_bytes;
        if !public_payment_list_fits_resolution_budget(
            current_endpoint_count,
            current_payload_bytes,
            endpoint_count,
            payload_bytes,
        ) {
            return false;
        }
        self.remaining_endpoints -= endpoint_count;
        self.remaining_payload_bytes -= payload_bytes;
        true
    }
}

fn public_payment_endpoint_load_failure(
    app_id: paykit_lib::PaykitAppId,
    error: paykit_lib::PaykitError,
) -> PublicPaymentEndpointLoadFailure {
    if paykit_lib::is_payment_list_limit_exceeded(&error) {
        return public_payment_endpoint_resource_limit_failure(app_id);
    }
    let (kind, context) = match error {
        paykit_lib::PaykitError::Transport { context, .. } => {
            (PublicPaymentEndpointLoadFailureKind::Transport, context)
        }
        paykit_lib::PaykitError::InvalidData { context, .. } => {
            (PublicPaymentEndpointLoadFailureKind::InvalidData, context)
        }
        paykit_lib::PaykitError::NotFound(context)
        | paykit_lib::PaykitError::Validation(context) => {
            (PublicPaymentEndpointLoadFailureKind::InvalidData, context)
        }
    };
    PublicPaymentEndpointLoadFailure {
        app_id,
        kind,
        context,
    }
}

pub(in crate::runtime) fn public_payment_endpoint_resource_limit_failure(
    app_id: paykit_lib::PaykitAppId,
) -> PublicPaymentEndpointLoadFailure {
    PublicPaymentEndpointLoadFailure {
        app_id,
        kind: PublicPaymentEndpointLoadFailureKind::ResourceLimit,
        context: format!(
            "public Payment Endpoint resolution is limited to {PUBLIC_PAYMENT_RESOLUTION_MAX_ENDPOINTS} endpoints and {PUBLIC_PAYMENT_RESOLUTION_MAX_PAYLOAD_BYTES} payload bytes"
        ),
    }
}

pub(in crate::runtime) fn public_payment_list_fits_resolution_budget(
    current_endpoint_count: usize,
    current_payload_bytes: usize,
    app_endpoint_count: usize,
    app_payload_bytes: usize,
) -> bool {
    current_endpoint_count.saturating_add(app_endpoint_count)
        <= PUBLIC_PAYMENT_RESOLUTION_MAX_ENDPOINTS
        && current_payload_bytes.saturating_add(app_payload_bytes)
            <= PUBLIC_PAYMENT_RESOLUTION_MAX_PAYLOAD_BYTES
}

pub(in crate::runtime) async fn load_public_payment_lists_with_budget<F, Fut>(
    app_ids: Vec<paykit_lib::PaykitAppId>,
    mut fetch: F,
) -> PublicPaymentListLoad
where
    F: FnMut(paykit_lib::PaykitAppId, usize, usize) -> Fut,
    Fut: Future<Output = paykit_lib::Result<paykit_lib::PaymentList>>,
{
    let mut load = PublicPaymentListLoad {
        payment_lists: Vec::new(),
        failures: Vec::new(),
        loaded_app_count: 0,
    };
    let mut budget = PublicPaymentResolutionBudget::default();
    for app_id in app_ids {
        let Some((max_endpoints, max_payload_bytes)) = budget.remaining_limits() else {
            load.failures
                .push(public_payment_endpoint_resource_limit_failure(app_id));
            continue;
        };
        let payment_list = match fetch(app_id.clone(), max_endpoints, max_payload_bytes).await {
            Ok(payment_list) => payment_list,
            Err(error) => {
                load.failures
                    .push(public_payment_endpoint_load_failure(app_id, error));
                continue;
            }
        };
        let app_payload_bytes = payment_list
            .payment_endpoints
            .values()
            .map(|payload| payload.as_str().len())
            .sum::<usize>();
        if !budget.consume(payment_list.payment_endpoints.len(), app_payload_bytes) {
            load.failures
                .push(public_payment_endpoint_resource_limit_failure(app_id));
            continue;
        }
        load.loaded_app_count += 1;
        load.payment_lists.push((app_id, payment_list));
    }
    load
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Resolve payable private endpoints for one counterparty.
    ///
    /// This method only reads Private Payment Lists and only invokes the
    /// private payment adapter callbacks. Public Payment Endpoints are never
    /// considered as fallback candidates. Pass the last consumed
    /// `private_payment_list_version` as `after_private_payment_list_version`
    /// to require a newer list. When the current list is not newer, the result
    /// is `WaitingForUpdatedPaymentList` and contains no payable endpoints.
    ///
    /// Versions are opaque local freshness tokens scoped to this SDK state and
    /// counterparty. The application owns
    /// consumption policy and should persist a payable result's version before
    /// submitting a payment that consumes the whole Private Payment List.
    pub async fn resolve_private_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
    ) -> Result<PrivateContactPaymentResolution> {
        self.resolve_private_contact_payment_with_terms(
            counterparty,
            amount,
            after_private_payment_list_version,
            None,
        )
        .await
    }

    /// Resolve private endpoints permitted by an actionable received Payment Request.
    ///
    /// The request amount is passed to the payment adapter. Candidates are
    /// restricted to the request's accepted Payment Endpoint Identifiers and,
    /// when present, its required payee App before adapter selection.
    pub async fn resolve_private_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        after_private_payment_list_version: Option<u64>,
    ) -> Result<PrivateContactPaymentResolution> {
        let terms = self
            .payment_request_resolution_terms(&counterparty, payment_request_id)
            .await?;
        let amount = payment_request_amount(&terms);
        self.resolve_private_contact_payment_with_terms(
            counterparty,
            Some(amount),
            after_private_payment_list_version,
            Some(terms),
        )
        .await
    }

    async fn resolve_private_contact_payment_with_terms(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
        payment_request_terms: Option<PaymentRequestTermsRecord>,
    ) -> Result<PrivateContactPaymentResolution> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        let private_live = session_access
            .as_ref()
            .map(|session| {
                session.private_link_capable_for_capabilities(PAYKIT_SESSION_CAPABILITIES)
            })
            .transpose()?
            .unwrap_or(false);
        let mut state = PrivatePaymentResolutionState::NoPrivateEndpoint;
        let mut private_allowed = identity.public_key.is_some();

        if private_allowed {
            private_allowed = if private_live {
                self.private_resolution_allowed_for_peer(&counterparty, &mut state)
                    .await?
            } else {
                self.cached_private_resolution_allowed_for_peer(&counterparty, &mut state)
                    .await?
            };
        }

        if private_allowed && private_live {
            match self
                .observe_remote_recovery_marker_for_cached_private_state(
                    &counterparty,
                    session_access.as_ref(),
                )
                .await
            {
                Ok(()) => {}
                Err(PaykitSdkError::RecoveryRequired { .. }) => {
                    state = PrivatePaymentResolutionState::RecoveryPending;
                    private_allowed = false;
                }
                Err(err) => return Err(err),
            }
        }

        if private_allowed && private_live {
            private_allowed = self
                .private_resolution_allowed_for_peer(&counterparty, &mut state)
                .await?;
        }

        let (app_registry, authorized_private_apps) = self
            .private_app_authorization_context(&counterparty)
            .await?;

        let mut private_views = if private_allowed {
            load_current_private_payment_lists(&self.storage, &counterparty).await?
        } else {
            Vec::new()
        };
        filter_private_views_by_authorized_apps(
            &mut private_views,
            authorized_private_apps.as_deref(),
        );
        if private_views
            .iter()
            .any(|view| !view.payment_endpoints.is_empty())
        {
            state = PrivatePaymentResolutionState::Available;
        }
        let mut candidate_batch = private_candidate_batch(
            &counterparty,
            &private_views,
            after_private_payment_list_version,
        )?;
        filter_private_candidate_batch_for_request(
            candidate_batch.as_mut(),
            payment_request_terms.as_ref(),
        );
        if candidate_batch
            .as_ref()
            .is_none_or(|batch| !batch.has_candidates())
            && private_live
            && state != PrivatePaymentResolutionState::RecoveryPending
        {
            match self
                .recover_private_candidates_for_resolution(
                    &counterparty,
                    authorized_private_apps.as_deref(),
                    after_private_payment_list_version,
                )
                .await?
            {
                PrivateRecoveryOutcome::Refreshed(refreshed) => {
                    candidate_batch = refreshed;
                    filter_private_candidate_batch_for_request(
                        candidate_batch.as_mut(),
                        payment_request_terms.as_ref(),
                    );
                    if candidate_batch
                        .as_ref()
                        .is_some_and(PrivatePaymentCandidateBatch::has_candidates)
                    {
                        state = PrivatePaymentResolutionState::Available;
                    }
                }
                PrivateRecoveryOutcome::Pending => {
                    state = PrivatePaymentResolutionState::RecoveryPending;
                }
                PrivateRecoveryOutcome::NotNeeded => {}
            }
        }

        if let (Some(candidate_batch), Some(app_registry)) =
            (candidate_batch.as_mut(), app_registry.as_ref())
        {
            candidate_batch.sort_by_app_preferences(app_registry);
        }

        let Some(candidate_batch) = candidate_batch else {
            return Ok(unresolved_private_resolution(false, state, None));
        };
        if !candidate_batch.is_newer_than(after_private_payment_list_version) {
            return Ok(waiting_for_updated_private_payment_list(
                state,
                candidate_batch.private_payment_list_version,
            ));
        }
        let candidates = candidate_batch.candidates();
        if candidates.is_empty() {
            if state != PrivatePaymentResolutionState::RecoveryPending {
                state = PrivatePaymentResolutionState::NoPrivateEndpoint;
            }
            return Ok(unresolved_private_resolution(
                false,
                state,
                Some(candidate_batch.private_payment_list_version),
            ));
        }

        self.resolve_private_candidate_batch(
            counterparty,
            amount,
            candidates,
            state,
            candidate_batch.private_payment_list_version,
        )
        .await
    }

    /// Resolve payable public Payment Endpoints for one counterparty.
    ///
    /// This method only reads public Pubky storage and only invokes the public
    /// payment adapter callbacks. Encrypted Link state is never consulted.
    pub async fn resolve_public_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
    ) -> Result<PublicContactPaymentResolution> {
        self.resolve_public_contact_payment_with_terms(counterparty, amount, None)
            .await
    }

    /// Resolve public endpoints permitted by an actionable received Payment Request.
    ///
    /// The request amount is passed to the payment adapter. Candidates are
    /// restricted to the request's accepted Payment Endpoint Identifiers and,
    /// when present, its required payee App before adapter selection.
    pub async fn resolve_public_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PublicContactPaymentResolution> {
        let terms = self
            .payment_request_resolution_terms(&counterparty, payment_request_id)
            .await?;
        let amount = payment_request_amount(&terms);
        self.resolve_public_contact_payment_with_terms(counterparty, Some(amount), Some(terms))
            .await
    }

    async fn resolve_public_contact_payment_with_terms(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        payment_request_terms: Option<PaymentRequestTermsRecord>,
    ) -> Result<PublicContactPaymentResolution> {
        let mut batch = self.public_payment_candidates(&counterparty).await?;
        filter_public_candidates_for_request(&mut batch.candidates, payment_request_terms.as_ref());
        if batch.candidates.is_empty() {
            return Ok(unresolved_public_resolution(
                false,
                batch.failures,
                batch.loaded_app_count,
            ));
        }

        self.resolve_public_candidate_batch_with_failures(
            counterparty,
            amount,
            batch.candidates,
            batch.failures,
            batch.loaded_app_count,
        )
        .await
    }

    /// Prepare private contact state, then resolve private endpoints.
    ///
    /// The SDK ensures or advances the Encrypted Link when a live session is
    /// available, drains currently available private send/receive work for the
    /// peer, and resolves only the counterparty's Private Payment List. Pass
    /// the last consumed list version as `after_private_payment_list_version`
    /// to return `WaitingForUpdatedPaymentList` until a newer list is received.
    pub async fn prepare_and_resolve_private_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
        max_advance_steps: u32,
    ) -> Result<PreparedPrivateContactPayment> {
        let (link_report, receive_report, outbound_report) = self
            .prepare_private_contact_payment(&counterparty, max_advance_steps)
            .await?;

        let resolution = self
            .resolve_private_contact_payment(
                counterparty,
                amount,
                after_private_payment_list_version,
            )
            .await?;

        Ok(PreparedPrivateContactPayment {
            resolution,
            link_report,
            receive_report,
            outbound_report,
        })
    }

    /// Prepare private state, then resolve endpoints permitted by a Payment Request.
    ///
    /// This performs the same bounded link and stream preparation as
    /// [`Self::prepare_and_resolve_private_contact_payment`], then applies the
    /// request amount, accepted endpoint identifiers, and required payee App.
    pub async fn prepare_and_resolve_private_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        after_private_payment_list_version: Option<u64>,
        max_advance_steps: u32,
    ) -> Result<PreparedPrivateContactPayment> {
        let (link_report, receive_report, outbound_report) = self
            .prepare_private_contact_payment(&counterparty, max_advance_steps)
            .await?;
        let resolution = self
            .resolve_private_payment_request(
                counterparty,
                payment_request_id,
                after_private_payment_list_version,
            )
            .await?;

        Ok(PreparedPrivateContactPayment {
            resolution,
            link_report,
            receive_report,
            outbound_report,
        })
    }

    async fn prepare_private_contact_payment(
        &self,
        counterparty: &PubkyPublicKey,
        max_advance_steps: u32,
    ) -> Result<(
        Option<LinkedPeerHandshakeReport>,
        Option<PrivateStreamIntakeReport>,
        Option<OutboundPrivateSendReport>,
    )> {
        let mut link_report = None;
        let mut receive_report = None;
        let mut outbound_report = None;

        if self.private_payment_preparation_is_available().await? {
            link_report = Some(
                self.ensure_link_with_peer(counterparty.clone(), max_advance_steps)
                    .await?,
            );
            for _ in 0..PREPARE_PRIVATE_PAYMENT_SYNC_ROUND_LIMIT {
                let outbound = self
                    .process_outbound_private_messages(counterparty.clone())
                    .await?;
                let outbound_progress = outbound_report_made_progress(&outbound);
                merge_outbound_report(&mut outbound_report, outbound);

                let received = self.receive_private_messages(counterparty.clone()).await?;
                let receive_progress = receive_report_made_progress(&received);
                merge_receive_report(&mut receive_report, received);

                if !outbound_progress && !receive_progress {
                    break;
                }
            }
        }

        Ok((link_report, receive_report, outbound_report))
    }

    async fn payment_request_resolution_terms(
        &self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestTermsRecord> {
        let record = self
            .load_payment_request_record(counterparty, payment_request_id)
            .await?;
        if record.local_role != Some(PaymentRequestLocalRole::Payer) {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "cannot resolve Payment Request {}: local identity is not the payer",
                    payment_request_id
                ),
                source: None,
            });
        }
        if !matches!(
            record.state,
            PaymentRequestLifecycleState::Proposed
                | PaymentRequestLifecycleState::Accepted
                | PaymentRequestLifecycleState::ActiveRecurring
        ) {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "cannot resolve Payment Request {} in state {:?}",
                    payment_request_id, record.state
                ),
                source: None,
            });
        }
        if record
            .payer_app_id
            .as_ref()
            .is_some_and(|payer_app_id| payer_app_id != &self.config.app_id)
        {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "cannot resolve Payment Request {}: another Paykit app owns the payer response",
                    payment_request_id
                ),
                source: None,
            });
        }
        self.ensure_payment_request_origin_app_authorized(
            counterparty,
            &record,
            &format!("resolve Payment Request {payment_request_id}"),
        )
        .await?;
        record.terms.ok_or_else(|| PaykitSdkError::Protocol {
            context: format!(
                "Payment Request {} terms are unavailable",
                payment_request_id
            ),
            source: None,
        })
    }

    async fn private_payment_preparation_is_available(&self) -> Result<bool> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        session_access
            .as_ref()
            .map(|session| {
                session.private_link_capable_for_capabilities(PAYKIT_SESSION_CAPABILITIES)
            })
            .transpose()
            .map(|capable| capable.unwrap_or(false))
    }

    async fn private_resolution_allowed_for_peer(
        &self,
        counterparty: &PubkyPublicKey,
        state: &mut PrivatePaymentResolutionState,
    ) -> Result<bool> {
        match self
            .ensure_peer_allows_private_automation(counterparty)
            .await
        {
            Ok(()) => Ok(true),
            Err(PaykitSdkError::RecoveryRequired { .. }) => {
                *state = PrivatePaymentResolutionState::RecoveryPending;
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    async fn cached_private_resolution_allowed_for_peer(
        &self,
        counterparty: &PubkyPublicKey,
        state: &mut PrivatePaymentResolutionState,
    ) -> Result<bool> {
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(counterparty).map(|peer| peer.state)))
            .await?;
        match peer_state {
            Some(LinkedPeerState::Linking | LinkedPeerState::RecoveryRequired) => {
                *state = PrivatePaymentResolutionState::RecoveryPending;
                Ok(false)
            }
            Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            }),
            _ => Ok(true),
        }
    }

    pub(super) async fn recover_private_candidates_for_resolution(
        &self,
        counterparty: &PubkyPublicKey,
        authorized_private_apps: Option<&[paykit_lib::PaykitAppId]>,
        after_private_payment_list_version: Option<u64>,
    ) -> Result<PrivateRecoveryOutcome> {
        let Some(identity) = self.storage.load_identity_state().await? else {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        };
        if identity.public_key.is_none() {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        }

        let (peer_state, has_active_link) = self
            .storage
            .transaction(|tx| {
                let peer = tx.linked_peer(counterparty);
                let link_state = tx.encrypted_link_state(counterparty);
                let has_active_link = link_state
                    .as_ref()
                    .and_then(|state| state.link_snapshot.as_ref())
                    .is_some();
                Ok((
                    peer.as_ref().map(|peer| peer.state.clone()),
                    has_active_link,
                ))
            })
            .await?;

        if matches!(
            peer_state,
            Some(LinkedPeerState::Linking | LinkedPeerState::RecoveryRequired)
        ) {
            return Ok(PrivateRecoveryOutcome::Pending);
        }

        if !has_active_link {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        }

        self.observe_remote_recovery_marker_for_cached_private_state(counterparty, None)
            .await?;

        match self.receive_private_messages(counterparty.clone()).await {
            Ok(_) => {
                let mut private_views =
                    load_current_private_payment_lists(&self.storage, counterparty).await?;
                filter_private_views_by_authorized_apps(
                    &mut private_views,
                    authorized_private_apps,
                );
                Ok(PrivateRecoveryOutcome::Refreshed(private_candidate_batch(
                    counterparty,
                    &private_views,
                    after_private_payment_list_version,
                )?))
            }
            Err(PaykitSdkError::Policy { .. })
            | Err(PaykitSdkError::RecoveryRequired { .. })
            | Err(PaykitSdkError::Transport { .. })
            | Err(PaykitSdkError::Protocol { .. }) => Ok(PrivateRecoveryOutcome::Pending),
            Err(err) => Err(err),
        }
    }

    async fn public_payment_candidates(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PublicPaymentCandidateLoad> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for public Payment Endpoint lookup"
                        .into(),
                    source: None,
                })?;
        let public_key = counterparty.to_public_key()?;
        let Some(registry) =
            paykit_lib::get_paykit_app_registry(&public_storage, &public_key).await?
        else {
            return Ok(PublicPaymentCandidateLoad::default());
        };
        let loaded = load_public_payment_lists_with_budget(
            public_app_load_order(&registry),
            |app_id, max_endpoints, max_payload_bytes| {
                let public_storage = &public_storage;
                let public_key = &public_key;
                async move {
                    paykit_lib::get_payment_list_with_limits(
                        public_storage,
                        public_key,
                        &app_id,
                        max_endpoints,
                        max_payload_bytes,
                    )
                    .await
                }
            },
        )
        .await;
        let mut endpoints = Vec::new();
        for (app_id, payment_list) in loaded.payment_lists {
            endpoints.extend(payment_list.payment_endpoints.into_iter().map(
                |(identifier, payload)| {
                    let preference_rank = app_preference_rank(&registry, &app_id, &identifier);
                    (
                        preference_rank,
                        PublicPaymentEndpointCandidate {
                            counterparty: counterparty.clone(),
                            app_id: app_id.clone(),
                            identifier: identifier.as_str().to_owned(),
                            payload: payload.into_inner(),
                        },
                    )
                },
            ));
        }
        endpoints.sort_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| {
                left.1
                    .app_id
                    .as_str()
                    .cmp(right.1.app_id.as_str())
                    .then_with(|| left.1.identifier.cmp(&right.1.identifier))
            })
        });
        Ok(PublicPaymentCandidateLoad {
            candidates: endpoints
                .into_iter()
                .map(|(_, candidate)| candidate)
                .collect(),
            failures: loaded.failures,
            loaded_app_count: loaded.loaded_app_count,
        })
    }

    pub(super) async fn private_app_authorization_context(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<(
        Option<paykit_lib::PaykitAppRegistry>,
        Option<Vec<paykit_lib::PaykitAppId>>,
    )> {
        let context = self
            .counterparty_app_authorization_context(counterparty)
            .await?;
        Ok((context.registry, context.private_apps))
    }

    async fn build_public_payable_endpoints(
        &self,
        payable: Vec<PublicPaymentEndpointCandidate>,
    ) -> Result<Vec<ResolvedPublicPaymentEndpoint>> {
        let mut endpoints = Vec::with_capacity(payable.len());
        for endpoint in payable {
            let target = self.payment.build_public_payment_target(&endpoint).await?;
            endpoints.push(ResolvedPublicPaymentEndpoint { endpoint, target });
        }
        Ok(endpoints)
    }

    async fn build_private_payable_endpoints(
        &self,
        payable: Vec<PrivatePaymentEndpointCandidate>,
    ) -> Result<Vec<ResolvedPrivatePaymentEndpoint>> {
        let mut endpoints = Vec::with_capacity(payable.len());
        for endpoint in payable {
            let target = self.payment.build_private_payment_target(&endpoint).await?;
            endpoints.push(ResolvedPrivatePaymentEndpoint { endpoint, target });
        }
        Ok(endpoints)
    }

    #[cfg(test)]
    pub(super) async fn resolve_public_candidate_batch(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        candidates: Vec<PublicPaymentEndpointCandidate>,
    ) -> Result<PublicContactPaymentResolution> {
        self.resolve_public_candidate_batch_with_failures(
            counterparty,
            amount,
            candidates,
            Vec::new(),
            1,
        )
        .await
    }

    async fn resolve_public_candidate_batch_with_failures(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        candidates: Vec<PublicPaymentEndpointCandidate>,
        failures: Vec<PublicPaymentEndpointLoadFailure>,
        loaded_app_count: usize,
    ) -> Result<PublicContactPaymentResolution> {
        let payable = self
            .payment
            .select_public_payment_endpoints(&PublicPaymentEndpointSelectionRequest {
                counterparty,
                amount,
                candidates: candidates.clone(),
            })
            .await?;
        let payable = public_payable_from_batch(&payable, &candidates)?;
        let payable_endpoints = self.build_public_payable_endpoints(payable).await?;
        if payable_endpoints.is_empty() {
            return Ok(unresolved_public_resolution(
                true,
                failures,
                loaded_app_count,
            ));
        }
        Ok(PublicContactPaymentResolution {
            status: PublicPaymentResolutionStatus::Payable,
            payable_endpoints,
            failures,
        })
    }

    pub(super) async fn resolve_private_candidate_batch(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        candidates: Vec<PrivatePaymentEndpointCandidate>,
        state: PrivatePaymentResolutionState,
        private_payment_list_version: u64,
    ) -> Result<PrivateContactPaymentResolution> {
        let payable = self
            .payment
            .select_private_payment_endpoints(&PrivatePaymentEndpointSelectionRequest {
                counterparty,
                amount,
                candidates: candidates.clone(),
            })
            .await?;
        let payable = private_payable_from_batch(&payable, &candidates)?;
        let payable_endpoints = self.build_private_payable_endpoints(payable).await?;
        if payable_endpoints.is_empty() {
            return Ok(unresolved_private_resolution(
                true,
                state,
                Some(private_payment_list_version),
            ));
        }
        Ok(PrivateContactPaymentResolution {
            status: PrivatePaymentResolutionStatus::Payable,
            state,
            private_payment_list_version: Some(private_payment_list_version),
            payable_endpoints,
        })
    }
}

pub(super) fn merge_outbound_report(
    current: &mut Option<OutboundPrivateSendReport>,
    mut report: OutboundPrivateSendReport,
) {
    let Some(current) = current.as_mut() else {
        *current = Some(report);
        return;
    };
    current.attempted.append(&mut report.attempted);
    current.sent.append(&mut report.sent);
    current.failed.append(&mut report.failed);
    current
        .reservation_cleanup_failures
        .append(&mut report.reservation_cleanup_failures);
    current
        .recovery_marker_failures
        .append(&mut report.recovery_marker_failures);
}

pub(super) fn merge_receive_report(
    current: &mut Option<PrivateStreamIntakeReport>,
    mut report: PrivateStreamIntakeReport,
) {
    let Some(current) = current.as_mut() else {
        *current = Some(report);
        return;
    };
    current.stream_item_ids.append(&mut report.stream_item_ids);
    current.event_conflicts.append(&mut report.event_conflicts);
}

fn outbound_report_made_progress(report: &OutboundPrivateSendReport) -> bool {
    !report.attempted.is_empty() || !report.sent.is_empty() || !report.failed.is_empty()
}

fn receive_report_made_progress(report: &PrivateStreamIntakeReport) -> bool {
    !report.stream_item_ids.is_empty() || !report.event_conflicts.is_empty()
}

pub(super) enum PrivateRecoveryOutcome {
    NotNeeded,
    Pending,
    Refreshed(Option<PrivatePaymentCandidateBatch>),
}
