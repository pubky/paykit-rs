use super::*;
use crate::PaymentAmountContext;

const PREPARE_PRIVATE_PAYMENT_SYNC_ROUND_LIMIT: usize = 8;

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
    /// Versions are opaque local freshness tokens scoped to this SDK state,
    /// counterparty, and counterparty receiver path. The application owns
    /// consumption policy and should persist a payable result's version before
    /// submitting a payment that consumes the whole Private Payment List.
    pub async fn resolve_private_contact_payment(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        amount: Option<PaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
    ) -> Result<PrivateContactPaymentResolution> {
        self.ensure_private_stream_classifications_normalized()
            .await?;
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        let private_live = session_access.is_some();
        let mut state = PrivatePaymentResolutionState::NoPrivateEndpoint;
        let mut private_allowed = identity.local_pubky_public_key.is_some();

        if private_allowed {
            private_allowed = if private_live {
                self.private_resolution_allowed_for_peer(
                    &counterparty,
                    &counterparty_receiver_path,
                    &mut state,
                )
                .await?
            } else {
                self.cached_private_resolution_allowed_for_peer(
                    &counterparty,
                    &counterparty_receiver_path,
                    &mut state,
                )
                .await?
            };
        }

        if private_allowed && private_live {
            match self
                .observe_remote_recovery_marker_for_cached_private_state(
                    &counterparty,
                    &counterparty_receiver_path,
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
                .private_resolution_allowed_for_peer(
                    &counterparty,
                    &counterparty_receiver_path,
                    &mut state,
                )
                .await?;
        }

        let private_view = if private_allowed {
            load_current_private_payment_list(
                &self.storage,
                &counterparty,
                &counterparty_receiver_path,
            )
            .await?
        } else {
            None
        };
        let mut candidate_batch = private_candidate_batch(
            &counterparty,
            &counterparty_receiver_path,
            private_view.as_ref(),
        )?;
        if candidate_batch
            .as_ref()
            .is_some_and(PrivatePaymentCandidateBatch::has_candidates)
        {
            state = PrivatePaymentResolutionState::Available;
        }

        if candidate_batch
            .as_ref()
            .is_none_or(|batch| !batch.has_candidates())
            && private_live
            && state != PrivatePaymentResolutionState::RecoveryPending
        {
            match self
                .recover_private_candidates_for_resolution(
                    &counterparty,
                    &counterparty_receiver_path,
                )
                .await?
            {
                PrivateRecoveryOutcome::Refreshed(refreshed) => {
                    candidate_batch = refreshed;
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

        let Some(candidate_batch) = candidate_batch else {
            return Ok(unresolved_private_resolution(false, state, None));
        };
        if !candidate_batch.is_newer_than(after_private_payment_list_version) {
            return Ok(waiting_for_updated_private_payment_list(
                state,
                candidate_batch.private_payment_list_version,
            ));
        }
        if !candidate_batch.has_candidates() {
            return Ok(unresolved_private_resolution(
                false,
                state,
                Some(candidate_batch.private_payment_list_version),
            ));
        }

        self.resolve_private_candidate_batch(
            counterparty,
            counterparty_receiver_path,
            amount,
            candidate_batch.candidates,
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
        counterparty_receiver_path: PaykitReceiverPath,
        amount: Option<PaymentAmountContext>,
    ) -> Result<PublicContactPaymentResolution> {
        let candidates = self
            .public_payment_candidates(&counterparty, &counterparty_receiver_path)
            .await?;
        if candidates.is_empty() {
            return Ok(unresolved_public_resolution(false));
        }

        self.resolve_public_candidate_batch(
            counterparty,
            counterparty_receiver_path,
            amount,
            candidates,
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
        counterparty_receiver_path: PaykitReceiverPath,
        amount: Option<PaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
        max_advance_steps: u32,
    ) -> Result<PreparedPrivateContactPayment> {
        self.ensure_private_stream_classifications_normalized()
            .await?;
        let mut link_report = None;
        let mut receive_report = None;
        let mut outbound_report = None;

        if self.private_payment_preparation_is_available().await? {
            link_report = Some(
                self.ensure_link_with_peer(
                    counterparty.clone(),
                    counterparty_receiver_path.clone(),
                    max_advance_steps,
                )
                .await?,
            );
            for _ in 0..PREPARE_PRIVATE_PAYMENT_SYNC_ROUND_LIMIT {
                let outbound = self
                    .process_outbound_private_messages(
                        counterparty.clone(),
                        counterparty_receiver_path.clone(),
                    )
                    .await?;
                let outbound_progress = outbound_report_made_progress(&outbound);
                merge_outbound_report(&mut outbound_report, outbound);

                let received = self
                    .receive_private_messages(
                        counterparty.clone(),
                        counterparty_receiver_path.clone(),
                    )
                    .await?;
                let receive_progress = receive_report_made_progress(&received);
                merge_receive_report(&mut receive_report, received);

                if !outbound_progress && !receive_progress {
                    break;
                }
            }
        }

        let resolution = self
            .resolve_private_contact_payment(
                counterparty,
                counterparty_receiver_path,
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

    async fn private_payment_preparation_is_available(&self) -> Result<bool> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        Ok(session_access.is_some())
    }

    async fn private_resolution_allowed_for_peer(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        state: &mut PrivatePaymentResolutionState,
    ) -> Result<bool> {
        match self
            .ensure_peer_allows_private_automation(counterparty, counterparty_receiver_path)
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
        counterparty_receiver_path: &PaykitReceiverPath,
        state: &mut PrivatePaymentResolutionState,
    ) -> Result<bool> {
        let peer_state = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .linked_peer(counterparty, counterparty_receiver_path)
                    .map(|peer| peer.state))
            })
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
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<PrivateRecoveryOutcome> {
        let Some(identity) = self.storage.load_identity_state().await? else {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        };
        if identity.local_pubky_public_key.is_none() {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        }

        let (peer_state, has_active_link) = self
            .storage
            .transaction(|tx| {
                let peer = tx.linked_peer(counterparty, counterparty_receiver_path);
                let link_state = tx.encrypted_link_state(counterparty, counterparty_receiver_path);
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

        self.observe_remote_recovery_marker_for_cached_private_state(
            counterparty,
            counterparty_receiver_path,
            None,
        )
        .await?;

        match self
            .receive_private_messages(counterparty.clone(), counterparty_receiver_path.clone())
            .await
        {
            Ok(_) => {
                let private_view = load_current_private_payment_list(
                    &self.storage,
                    counterparty,
                    counterparty_receiver_path,
                )
                .await?;
                Ok(PrivateRecoveryOutcome::Refreshed(private_candidate_batch(
                    counterparty,
                    counterparty_receiver_path,
                    private_view.as_ref(),
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
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<PublicPaymentEndpointCandidate>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for public Payment Endpoint lookup"
                        .into(),
                    source: None,
                })?;
        let payment_list = paykit_lib::get_payment_list(
            &public_storage,
            &counterparty.to_public_key()?,
            counterparty_receiver_path,
        )
        .await?;
        let mut endpoints = payment_list
            .payment_endpoints
            .into_iter()
            .map(|(identifier, payload)| PublicPaymentEndpointCandidate {
                counterparty: counterparty.clone(),
                counterparty_receiver_path: counterparty_receiver_path.clone(),
                identifier: identifier.as_str().to_owned(),
                payload: payload.into_inner(),
            })
            .collect::<Vec<_>>();
        endpoints.sort_by(|left, right| left.identifier.cmp(&right.identifier));
        Ok(endpoints)
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

    pub(super) async fn resolve_public_candidate_batch(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        amount: Option<PaymentAmountContext>,
        candidates: Vec<PublicPaymentEndpointCandidate>,
    ) -> Result<PublicContactPaymentResolution> {
        let payable = self
            .payment
            .select_public_payment_endpoints(&PublicPaymentEndpointSelectionRequest {
                counterparty,
                counterparty_receiver_path,
                amount,
                candidates: candidates.clone(),
            })
            .await?;
        let payable = public_payable_from_batch(&payable, &candidates)?;
        let payable_endpoints = self.build_public_payable_endpoints(payable).await?;
        if payable_endpoints.is_empty() {
            return Ok(unresolved_public_resolution(true));
        }
        Ok(PublicContactPaymentResolution {
            status: PublicPaymentResolutionStatus::Payable,
            payable_endpoints,
        })
    }

    pub(super) async fn resolve_private_candidate_batch(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        amount: Option<PaymentAmountContext>,
        candidates: Vec<PrivatePaymentEndpointCandidate>,
        state: PrivatePaymentResolutionState,
        private_payment_list_version: u64,
    ) -> Result<PrivateContactPaymentResolution> {
        let payable = self
            .payment
            .select_private_payment_endpoints(&PrivatePaymentEndpointSelectionRequest {
                counterparty,
                counterparty_receiver_path,
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

fn private_candidate_batch(
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    view: Option<&PrivatePaymentListView>,
) -> Result<Option<PrivatePaymentCandidateBatch>> {
    let Some(view) = view else {
        return Ok(None);
    };
    let private_payment_list_version =
        view.latest_stream_item_id
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: "current Private Payment List has no stream item id".into(),
                source: None,
            })?;
    let mut candidates = view
        .payment_endpoints
        .iter()
        .map(|(identifier, payload)| PrivatePaymentEndpointCandidate {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: counterparty_receiver_path.clone(),
            identifier: identifier.clone(),
            payload: payload.clone(),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.identifier.cmp(&right.identifier));
    Ok(Some(PrivatePaymentCandidateBatch {
        private_payment_list_version,
        candidates,
    }))
}

pub(super) struct PrivatePaymentCandidateBatch {
    private_payment_list_version: u64,
    candidates: Vec<PrivatePaymentEndpointCandidate>,
}

impl PrivatePaymentCandidateBatch {
    fn has_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    fn is_newer_than(&self, previous_version: Option<u64>) -> bool {
        previous_version.is_none_or(|version| self.private_payment_list_version > version)
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
    current
        .parked_unsupported
        .append(&mut report.parked_unsupported);
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
    // Parked entries intentionally do not count as progress: a parked
    // unknown-kind head blocks its peer's queue without state changing, so
    // counting it would spin the sync round loop without draining anything.
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

fn unresolved_public_resolution(had_candidates: bool) -> PublicContactPaymentResolution {
    PublicContactPaymentResolution {
        status: if had_candidates {
            PublicPaymentResolutionStatus::UnsupportedEndpoint
        } else {
            PublicPaymentResolutionStatus::NoEndpoint
        },
        payable_endpoints: Vec::new(),
    }
}

fn unresolved_private_resolution(
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

fn waiting_for_updated_private_payment_list(
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

pub(super) fn public_payable_from_batch(
    selected: &[PublicPaymentEndpointCandidate],
    candidates: &[PublicPaymentEndpointCandidate],
) -> Result<Vec<PublicPaymentEndpointCandidate>> {
    payable_from_batch(selected, candidates)
}

pub(super) fn private_payable_from_batch(
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
