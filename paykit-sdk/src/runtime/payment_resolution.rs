use super::*;
use crate::PaymentAmountContext;

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
        let mut private_allowed = self.config.private_sharing != PrivateSharingPolicy::Disabled
            && identity.public_key.is_some();
        let mut private_recovery_pending = false;
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            private_allowed = match self
                .ensure_peer_allows_private_automation(&request.counterparty)
                .await
            {
                Ok(()) => true,
                Err(PaykitSdkError::RecoveryRequired(_)) => {
                    private_recovery_pending = true;
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
                    private_recovery_pending = true;
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
                    private_recovery_pending = true;
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
        let had_private_candidates = !candidates.is_empty();

        if candidates.is_empty() && !request.include_public_endpoints {
            if private_recovery_pending {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::PrivateRecoveryPending,
                ));
            }
            let mut public_only_session = false;
            match self
                .recover_private_candidates_for_resolution(&request.counterparty)
                .await?
            {
                PrivateRecoveryOutcome::Refreshed(refreshed_candidates)
                    if !refreshed_candidates.is_empty() =>
                {
                    candidates = refreshed_candidates;
                }
                PrivateRecoveryOutcome::Pending => {
                    return Ok(status_resolution(
                        ContactPaymentResolutionStatus::PrivateRecoveryPending,
                    ));
                }
                PrivateRecoveryOutcome::PublicOnly => {
                    public_only_session = true;
                }
                PrivateRecoveryOutcome::NotNeeded | PrivateRecoveryOutcome::Refreshed(_) => {}
            }
            if public_only_session {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::PublicOnlySession,
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
            return Ok(unresolved_resolution(
                had_private_candidates,
                private_recovery_pending,
            ));
        }

        self.resolve_candidate_batch(request.counterparty, request.amount, candidates)
            .await
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

        let (peer_state, has_active_link, link_generation) = self
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
                    has_active_link,
                    link_generation,
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
            return Ok(payable_resolution(payable_endpoints));
        }

        Ok(unresolved_resolution(true, false))
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

pub(super) enum PrivateRecoveryOutcome {
    NotNeeded,
    Pending,
    PublicOnly,
    Refreshed(Vec<PaymentEndpointCandidate>),
}
fn payable_resolution(payable_endpoints: Vec<ResolvedPaymentEndpoint>) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: ContactPaymentResolutionStatus::Payable,
        payable_endpoints,
    }
}

fn status_resolution(status: ContactPaymentResolutionStatus) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status,
        payable_endpoints: Vec::new(),
    }
}

fn unresolved_resolution(
    had_candidates: bool,
    private_recovery_pending: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: if private_recovery_pending {
            ContactPaymentResolutionStatus::PrivateRecoveryPending
        } else if had_candidates {
            ContactPaymentResolutionStatus::UnsupportedEndpoint
        } else {
            ContactPaymentResolutionStatus::NoEndpoint
        },
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
