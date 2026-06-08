use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Resolve a payable endpoint for one counterparty.
    pub async fn resolve_contact_payment(
        &self,
        request: ContactPaymentResolutionRequest,
    ) -> Result<ContactPaymentResolution> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        let mut evaluations = Vec::new();
        let mut private_allowed = self.config.private_sharing != PrivateSharingPolicy::Disabled
            && identity.public_key.is_some();
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            private_allowed = match self
                .ensure_peer_allows_private_automation(&request.counterparty)
                .await
            {
                Ok(()) => true,
                Err(PaykitSdkError::RecoveryRequired(_)) => false,
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
                if self.config.public_fallback == PublicFallbackPolicy::Disabled {
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
                Err(PaykitSdkError::RecoveryRequired(_)) => false,
                Err(err) => return Err(err),
            };
        }
        let private_view = if private_allowed {
            load_current_private_payment_list(&self.storage, &request.counterparty).await?
        } else {
            None
        };
        let private_candidates = private_candidates(&request.counterparty, private_view.as_ref());

        if !private_candidates.is_empty() {
            let selection = self
                .payment
                .select_payment_endpoint(&PaymentEndpointSelectionRequest {
                    counterparty: request.counterparty.clone(),
                    amount: request.amount.clone(),
                    candidates: private_candidates.clone(),
                })
                .await?;
            let selected = selected_from_batch(&selection, &private_candidates)?;
            evaluations.extend(selection.evaluations);
            if let Some(selected) = selected {
                let target = self.payment.build_payment_target(&selected).await?;
                return Ok(payable_resolution(selected, target, evaluations, false));
            }
        }

        let mut public_only_session = false;
        if self.config.public_fallback != PublicFallbackPolicy::WhenPrivateUnavailable {
            match self
                .recover_private_candidates_for_resolution(&request.counterparty)
                .await?
            {
                PrivateRecoveryOutcome::Refreshed(refreshed_candidates)
                    if !refreshed_candidates.is_empty() =>
                {
                    let selection = self
                        .payment
                        .select_payment_endpoint(&PaymentEndpointSelectionRequest {
                            counterparty: request.counterparty.clone(),
                            amount: request.amount.clone(),
                            candidates: refreshed_candidates.clone(),
                        })
                        .await?;
                    let selected = selected_from_batch(&selection, &refreshed_candidates)?;
                    evaluations.extend(selection.evaluations);
                    if let Some(selected) = selected {
                        let target = self.payment.build_payment_target(&selected).await?;
                        return Ok(payable_resolution(selected, target, evaluations, false));
                    }
                }
                PrivateRecoveryOutcome::Pending => {
                    return Ok(status_resolution(
                        ContactPaymentResolutionStatus::PrivateRecoveryPending,
                        evaluations,
                        false,
                    ));
                }
                PrivateRecoveryOutcome::PublicOnly => {
                    public_only_session = true;
                }
                PrivateRecoveryOutcome::NotNeeded | PrivateRecoveryOutcome::Refreshed(_) => {}
            }
        }

        if self.config.public_fallback == PublicFallbackPolicy::Disabled {
            if public_only_session {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::PublicOnlySession,
                    evaluations,
                    false,
                ));
            }
            return Ok(unresolved_resolution(
                !private_candidates.is_empty(),
                evaluations,
                false,
            ));
        }

        let public_candidates = self
            .public_payment_candidates(&request.counterparty)
            .await?;
        if public_candidates.is_empty() {
            if public_only_session {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::PublicOnlySession,
                    evaluations,
                    false,
                ));
            }
            return Ok(unresolved_resolution(
                !private_candidates.is_empty(),
                evaluations,
                false,
            ));
        }

        let selection = self
            .payment
            .select_payment_endpoint(&PaymentEndpointSelectionRequest {
                counterparty: request.counterparty,
                amount: request.amount,
                candidates: public_candidates.clone(),
            })
            .await?;
        let selected = selected_from_batch(&selection, &public_candidates)?;
        evaluations.extend(selection.evaluations);
        if let Some(selected) = selected {
            let target = self.payment.build_payment_target(&selected).await?;
            return Ok(payable_resolution(selected, target, evaluations, true));
        }

        Ok(unresolved_resolution(true, evaluations, true))
    }

    pub(super) async fn recover_private_candidates_for_resolution(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PrivateRecoveryOutcome> {
        if self.config.private_sharing == PrivateSharingPolicy::Disabled {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        }

        let Some(identity) = self.storage.load_identity_state().await? else {
            return Ok(PrivateRecoveryOutcome::PublicOnly);
        };
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(PrivateRecoveryOutcome::PublicOnly);
        }

        let (peer_state, peer_last_sync_at, has_active_link, link_generation) = self
            .storage
            .transaction(|tx| {
                let peer = tx.linked_peer(counterparty);
                let link_state = tx.encrypted_link_state(counterparty);
                let has_active_link = link_state
                    .as_ref()
                    .and_then(|state| state.link_snapshot.as_ref())
                    .is_some();
                let link_generation = link_state.as_ref().map(|state| state.generation);
                Ok((
                    peer.as_ref().map(|peer| peer.state.clone()),
                    peer.and_then(|peer| peer.last_sync_at),
                    has_active_link,
                    link_generation,
                ))
            })
            .await?;

        if matches!(
            peer_state,
            Some(LinkedPeerState::Linking | LinkedPeerState::RecoveryRequired)
        ) {
            if peer_last_sync_at
                .map(|last_sync_at| self.private_recovery_window_open(last_sync_at))
                .transpose()?
                .unwrap_or(false)
            {
                return Ok(PrivateRecoveryOutcome::Pending);
            }

            if matches!(peer_state, Some(LinkedPeerState::RecoveryRequired)) {
                return Ok(PrivateRecoveryOutcome::NotNeeded);
            }
        }

        if has_active_link {
            if let Err(err) = self
                .observe_remote_recovery_marker_for_cached_private_state(counterparty, None)
                .await
            {
                if self.config.public_fallback == PublicFallbackPolicy::Disabled {
                    return Err(err);
                }
                return Ok(PrivateRecoveryOutcome::NotNeeded);
            }

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
                    self.mark_private_recovery_pending_and_publish_marker(
                        counterparty,
                        link_generation,
                    )
                    .await?;
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

pub(super) fn should_mark_link_recovery_required(err: &PaykitSdkError) -> bool {
    matches!(
        err,
        PaykitSdkError::Transport { .. }
            | PaykitSdkError::NotFound(_)
            | PaykitSdkError::Protocol(_)
            | PaykitSdkError::RecoveryRequired(_)
    )
}
pub(super) enum PrivateRecoveryOutcome {
    NotNeeded,
    Pending,
    PublicOnly,
    Refreshed(Vec<PaymentEndpointCandidate>),
}
fn payable_resolution(
    selected: PaymentEndpointCandidate,
    payment_target: PaymentTarget,
    evaluations: Vec<PaymentEndpointEvaluation>,
    used_public_fallback: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: ContactPaymentResolutionStatus::Payable,
        selected_endpoint: Some(selected),
        payment_target: Some(payment_target),
        evaluations,
        used_public_fallback,
    }
}

fn status_resolution(
    status: ContactPaymentResolutionStatus,
    evaluations: Vec<PaymentEndpointEvaluation>,
    used_public_fallback: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status,
        selected_endpoint: None,
        payment_target: None,
        evaluations,
        used_public_fallback,
    }
}

fn unresolved_resolution(
    had_candidates: bool,
    evaluations: Vec<PaymentEndpointEvaluation>,
    used_public_fallback: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: if had_candidates {
            ContactPaymentResolutionStatus::UnsupportedEndpoint
        } else {
            ContactPaymentResolutionStatus::NoEndpoint
        },
        selected_endpoint: None,
        payment_target: None,
        evaluations,
        used_public_fallback,
    }
}

pub(super) fn selected_from_batch(
    selection: &PaymentEndpointSelection,
    candidates: &[PaymentEndpointCandidate],
) -> Result<Option<PaymentEndpointCandidate>> {
    for evaluation in &selection.evaluations {
        if !candidates.contains(&evaluation.candidate) {
            return Err(PaykitSdkError::Protocol(
                "PaymentAdapter evaluated an endpoint that was not in the candidate batch".into(),
            ));
        }
    }
    let Some(selected) = selection.selected.as_ref() else {
        return Ok(None);
    };
    if !candidates.contains(selected) {
        return Err(PaykitSdkError::Protocol(
            "PaymentAdapter selected an endpoint that was not in the candidate batch".into(),
        ));
    }
    let selected_evaluations = selection
        .evaluations
        .iter()
        .filter(|evaluation| evaluation.candidate == *selected)
        .collect::<Vec<_>>();
    if selected_evaluations.is_empty() {
        return Err(PaykitSdkError::Protocol(
            "PaymentAdapter selected an endpoint without a matching evaluation".into(),
        ));
    }
    if selected_evaluations
        .iter()
        .any(|evaluation| evaluation.compatibility != EndpointCompatibility::Payable)
    {
        return Err(PaykitSdkError::Protocol(
            "PaymentAdapter selected an endpoint that was not evaluated as payable".into(),
        ));
    }
    Ok(Some(selected.clone()))
}
