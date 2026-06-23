use super::*;
use crate::PaymentAmountContext;

const PREPARE_CONTACT_PAYMENT_MAX_SYNC_ROUNDS: usize = 4;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Resolve payable endpoints for one counterparty.
    ///
    /// This uses cached private state first. Call
    /// [`receive_private_messages`](Self::receive_private_messages) before
    /// resolving when the app needs the freshest Private Payment List.
    pub async fn resolve_contact_payment(
        &self,
        request: ContactPaymentResolutionRequest,
    ) -> Result<ContactPaymentResolution> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        let mut private_allowed = identity.public_key.is_some();
        let mut private_state = if private_allowed {
            ContactPaymentResolutionPrivateState::NoPrivateEndpoint
        } else {
            ContactPaymentResolutionPrivateState::PublicOnlySession
        };
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            private_allowed = match self
                .ensure_peer_allows_private_automation(&request.counterparty)
                .await
            {
                Ok(()) => true,
                Err(PaykitSdkError::RecoveryRequired(_)) => {
                    private_state = ContactPaymentResolutionPrivateState::RecoveryPending;
                    false
                }
                Err(err) => return Err(err),
            };
        }
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            if let Err(err) = self
                .observe_remote_recovery_marker_for_cached_private_state(
                    &request.counterparty,
                    session_access.as_ref(),
                )
                .await
            {
                if matches!(err, PaykitSdkError::RecoveryRequired(_)) {
                    private_state = ContactPaymentResolutionPrivateState::RecoveryPending;
                } else if !request.include_public_endpoints {
                    return Err(err);
                }
                private_allowed = false;
            }
        }
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            private_allowed = match self
                .ensure_peer_allows_private_automation(&request.counterparty)
                .await
            {
                Ok(()) => true,
                Err(PaykitSdkError::RecoveryRequired(_)) => {
                    private_state = ContactPaymentResolutionPrivateState::RecoveryPending;
                    false
                }
                Err(err) => return Err(err),
            };
        }
        let private_view = if private_allowed {
            load_current_private_payment_list(&self.storage, &request.counterparty).await?
        } else {
            None
        };
        let mut candidates = private_candidates(&request.counterparty, private_view.as_ref());
        if !candidates.is_empty() {
            private_state = ContactPaymentResolutionPrivateState::Available;
        } else if private_allowed
            && identity.capability != PubkyIdentityCapability::PrivateLinkCapable
        {
            private_state = ContactPaymentResolutionPrivateState::PublicOnlySession;
        }
        let had_private_candidates = !candidates.is_empty();

        if candidates.is_empty() && private_allowed {
            if private_state == ContactPaymentResolutionPrivateState::RecoveryPending {
                if !request.include_public_endpoints {
                    return Ok(status_resolution(
                        ContactPaymentResolutionStatus::NoEndpoint,
                        private_state,
                    ));
                }
            } else {
                match self
                    .recover_private_candidates_for_resolution(&request.counterparty)
                    .await?
                {
                    PrivateRecoveryOutcome::Refreshed(refreshed_candidates)
                        if !refreshed_candidates.is_empty() =>
                    {
                        candidates = refreshed_candidates;
                        private_state = ContactPaymentResolutionPrivateState::Available;
                    }
                    PrivateRecoveryOutcome::Pending => {
                        private_state = ContactPaymentResolutionPrivateState::RecoveryPending;
                        if !request.include_public_endpoints {
                            return Ok(status_resolution(
                                ContactPaymentResolutionStatus::NoEndpoint,
                                private_state,
                            ));
                        }
                    }
                    PrivateRecoveryOutcome::PublicOnly => {
                        private_state = ContactPaymentResolutionPrivateState::PublicOnlySession;
                    }
                    PrivateRecoveryOutcome::NotNeeded | PrivateRecoveryOutcome::Refreshed(_) => {}
                }
            }
            if !request.include_public_endpoints
                && private_state == ContactPaymentResolutionPrivateState::PublicOnlySession
            {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::NoEndpoint,
                    private_state,
                ));
            }
        }

        if request.include_public_endpoints {
            candidates.extend(
                self.public_payment_candidates(&request.counterparty)
                    .await?,
            );
        }

        if candidates.is_empty() {
            return Ok(unresolved_resolution(had_private_candidates, private_state));
        }

        self.resolve_candidate_batch(
            request.counterparty,
            request.amount,
            candidates,
            private_state,
        )
        .await
    }

    /// Resolve payable private endpoints for one counterparty.
    pub async fn resolve_private_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
    ) -> Result<ContactPaymentResolution> {
        self.resolve_contact_payment(ContactPaymentResolutionRequest {
            counterparty,
            amount,
            include_public_endpoints: false,
        })
        .await
    }

    /// Resolve payable public endpoints for one counterparty.
    pub async fn resolve_public_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
    ) -> Result<ContactPaymentResolution> {
        let candidates = self.public_payment_candidates(&counterparty).await?;
        if candidates.is_empty() {
            return Ok(status_resolution(
                ContactPaymentResolutionStatus::NoEndpoint,
                ContactPaymentResolutionPrivateState::NoPrivateEndpoint,
            ));
        }
        self.resolve_candidate_batch(
            counterparty,
            amount,
            candidates,
            ContactPaymentResolutionPrivateState::NoPrivateEndpoint,
        )
        .await
    }

    /// Prepare private contact state, then resolve payable endpoints.
    ///
    /// This is the app-facing "pay contact" workflow. It refreshes the live
    /// session capability, ensures or advances the private link when possible,
    /// drains currently available private send/receive work for the peer, then
    /// resolves endpoints private-first. Public endpoints are included only
    /// when `include_public_endpoints` is true; in that mode, private
    /// preparation failures are reported and public fallback can still be
    /// returned.
    pub async fn prepare_and_resolve_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        include_public_endpoints: bool,
        max_advance_steps: u32,
    ) -> Result<PreparedContactPayment> {
        let mut link_report = None;
        let mut receive_report = None;
        let mut outbound_report = None;
        let mut private_error = None;

        if self.private_payment_preparation_is_available().await? {
            match self
                .ensure_link_with_peer(counterparty.clone(), max_advance_steps)
                .await
            {
                Ok(report) => {
                    link_report = Some(report);
                    for _ in 0..PREPARE_CONTACT_PAYMENT_MAX_SYNC_ROUNDS {
                        match self
                            .process_outbound_private_messages(counterparty.clone())
                            .await
                        {
                            Ok(report) => {
                                let outbound_progress = outbound_report_made_progress(&report);
                                merge_outbound_report(&mut outbound_report, report);

                                match self.receive_private_messages(counterparty.clone()).await {
                                    Ok(report) => {
                                        let receive_progress =
                                            receive_report_made_progress(&report);
                                        merge_receive_report(&mut receive_report, report);
                                        if !outbound_progress && !receive_progress {
                                            break;
                                        }
                                    }
                                    Err(err) => {
                                        if !include_public_endpoints {
                                            return Err(err);
                                        }
                                        private_error = Some(err.to_string());
                                        break;
                                    }
                                }
                            }
                            Err(err) => {
                                if !include_public_endpoints {
                                    return Err(err);
                                }
                                private_error = Some(err.to_string());
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    if !include_public_endpoints {
                        return Err(err);
                    }
                    private_error = Some(err.to_string());
                }
            }
        }

        let mut resolution = self
            .resolve_contact_payment(ContactPaymentResolutionRequest {
                counterparty,
                amount,
                include_public_endpoints,
            })
            .await?;
        prefer_private_endpoints(&mut resolution);

        Ok(PreparedContactPayment {
            resolution,
            link_report,
            receive_report,
            outbound_report,
            private_error,
        })
    }

    async fn private_payment_preparation_is_available(&self) -> Result<bool> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        Ok(identity.capability == PubkyIdentityCapability::PrivateLinkCapable)
    }

    pub(super) async fn recover_private_candidates_for_resolution(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PrivateRecoveryOutcome> {
        let Some(identity) = self.storage.load_identity_state().await? else {
            return Ok(PrivateRecoveryOutcome::PublicOnly);
        };
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(PrivateRecoveryOutcome::PublicOnly);
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

        if has_active_link {
            self.observe_remote_recovery_marker_for_cached_private_state(counterparty, None)
                .await?;

            match self.receive_private_messages(counterparty.clone()).await {
                Ok(_) => {
                    let private_view =
                        load_current_private_payment_list(&self.storage, counterparty).await?;
                    return Ok(PrivateRecoveryOutcome::Refreshed(private_candidates(
                        counterparty,
                        private_view.as_ref(),
                    )));
                }
                Err(PaykitSdkError::Policy(_)) => return Ok(PrivateRecoveryOutcome::Pending),
                Err(PaykitSdkError::Identity { .. }) => {
                    return Ok(PrivateRecoveryOutcome::PublicOnly)
                }
                Err(PaykitSdkError::RecoveryRequired(_))
                | Err(PaykitSdkError::Transport { .. })
                | Err(PaykitSdkError::Protocol(_)) => {
                    return Ok(PrivateRecoveryOutcome::Pending);
                }
                Err(err) => return Err(err),
            }
        }

        Ok(PrivateRecoveryOutcome::NotNeeded)
    }

    async fn public_payment_candidates(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PaymentEndpointCandidate>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for public Payment Endpoint lookup"
                        .into(),
                    source: None,
                })?;
        let payment_list =
            paykit_lib::get_payment_list(&public_storage, &counterparty.to_public_key()?).await?;
        let mut endpoints = payment_list
            .payment_endpoints
            .into_iter()
            .map(|(identifier, payload)| PaymentEndpointCandidate {
                counterparty: counterparty.clone(),
                source: PaymentEndpointSource::PublicPaymentEndpoint,
                identifier: identifier.as_str().to_owned(),
                payload: payload.into_inner(),
            })
            .collect::<Vec<_>>();
        endpoints.sort_by(|left, right| left.identifier.cmp(&right.identifier));
        Ok(endpoints)
    }

    async fn build_payable_endpoints(
        &self,
        payable: Vec<PaymentEndpointCandidate>,
    ) -> Result<Vec<ResolvedPaymentEndpoint>> {
        let mut endpoints = Vec::with_capacity(payable.len());
        for endpoint in payable {
            let target = self.payment.build_payment_target(&endpoint).await?;
            endpoints.push(ResolvedPaymentEndpoint { endpoint, target });
        }
        Ok(endpoints)
    }

    pub(super) async fn resolve_candidate_batch(
        &self,
        counterparty: PubkyPublicKey,
        amount: Option<PaymentAmountContext>,
        candidates: Vec<PaymentEndpointCandidate>,
        private_state: ContactPaymentResolutionPrivateState,
    ) -> Result<ContactPaymentResolution> {
        let payable = self
            .payment
            .select_payment_endpoints(&PaymentEndpointSelectionRequest {
                counterparty,
                amount,
                candidates: candidates.clone(),
            })
            .await?;
        let payable = payable_from_batch(&payable, &candidates)?;
        let payable_endpoints = self.build_payable_endpoints(payable).await?;
        if !payable_endpoints.is_empty() {
            return Ok(payable_resolution(payable_endpoints, private_state));
        }

        Ok(unresolved_resolution(true, private_state))
    }
}

fn private_candidates(
    counterparty: &PubkyPublicKey,
    view: Option<&PrivatePaymentListView>,
) -> Vec<PaymentEndpointCandidate> {
    let Some(view) = view else {
        return Vec::new();
    };
    let mut candidates = view
        .payment_endpoints
        .iter()
        .map(|(identifier, payload)| PaymentEndpointCandidate {
            counterparty: counterparty.clone(),
            source: PaymentEndpointSource::PrivatePaymentList,
            identifier: identifier.clone(),
            payload: payload.clone(),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.identifier.cmp(&right.identifier));
    candidates
}

pub(super) fn prefer_private_endpoints(resolution: &mut ContactPaymentResolution) {
    resolution.payable_endpoints.sort_by_key(|endpoint| {
        if endpoint.endpoint.source == PaymentEndpointSource::PrivatePaymentList {
            0
        } else {
            1
        }
    });
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
    PublicOnly,
    Refreshed(Vec<PaymentEndpointCandidate>),
}
fn payable_resolution(
    payable_endpoints: Vec<ResolvedPaymentEndpoint>,
    private_state: ContactPaymentResolutionPrivateState,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: ContactPaymentResolutionStatus::Payable,
        private_state,
        payable_endpoints,
    }
}

fn status_resolution(
    status: ContactPaymentResolutionStatus,
    private_state: ContactPaymentResolutionPrivateState,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status,
        private_state,
        payable_endpoints: Vec::new(),
    }
}

fn unresolved_resolution(
    had_candidates: bool,
    private_state: ContactPaymentResolutionPrivateState,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: if had_candidates {
            ContactPaymentResolutionStatus::UnsupportedEndpoint
        } else {
            ContactPaymentResolutionStatus::NoEndpoint
        },
        private_state,
        payable_endpoints: Vec::new(),
    }
}

pub(super) fn payable_from_batch(
    selected: &[PaymentEndpointCandidate],
    candidates: &[PaymentEndpointCandidate],
) -> Result<Vec<PaymentEndpointCandidate>> {
    let mut payable = Vec::with_capacity(selected.len());
    for candidate in selected {
        if !candidates.contains(candidate) {
            return Err(PaykitSdkError::Protocol(
                "PaymentAdapter returned a payable endpoint that was not in the candidate batch"
                    .into(),
            ));
        }
        if payable.contains(candidate) {
            return Err(PaykitSdkError::Protocol(
                "PaymentAdapter returned duplicate payable endpoints".into(),
            ));
        }
        payable.push(candidate.clone());
    }
    Ok(payable)
}
