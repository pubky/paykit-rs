use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Return inbound Payment Requests received from one counterparty.
    ///
    /// This view is useful for inspecting proposals received from a
    /// counterparty. For normal app state, including responses to locally
    /// proposed requests, use [`Self::payment_requests_with`], which merges
    /// inbound events with local outbound context. Malformed recognized Payment
    /// Request events without a valid `payment_request_id` stay in the raw
    /// private stream log and cannot be attached to a request-scoped record.
    pub async fn received_payment_requests_from(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.local_pubky_public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty, counterparty_receiver_path)
            .await?;
        let mut records = derive_received_payment_request_records(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            self.clock.now(),
        )
        .await?;
        self.mark_recovery_required_payment_request_records(
            counterparty,
            counterparty_receiver_path,
            &mut records,
        )
        .await?;
        Ok(records)
    }

    /// Return Payment Requests involving one counterparty.
    ///
    /// Results combine received private-stream events and local outbound
    /// Payment Request events. They are returned newest-first.
    pub async fn payment_requests_with(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.local_pubky_public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty, counterparty_receiver_path)
            .await?;
        let mut records = derive_payment_request_records(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            self.clock.now(),
        )
        .await?;
        self.mark_recovery_required_payment_request_records(
            counterparty,
            counterparty_receiver_path,
            &mut records,
        )
        .await?;
        Ok(records)
    }

    /// Return Payment Requests matching a local SDK filter.
    ///
    /// A filter without a counterparty lists across all non-blocked
    /// counterparties that have inbound or outbound Payment Request activity.
    /// Results are returned newest-first.
    pub async fn list_payment_requests(
        &self,
        filter: PaymentRequestFilter,
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.local_pubky_public_key.is_none() {
            return Ok(Vec::new());
        }
        let now = self.clock.now();

        let counterparties = match (&filter.counterparty, &filter.counterparty_receiver_path) {
            (Some(counterparty), Some(receiver_path)) => {
                self.ensure_peer_not_blocked(counterparty, receiver_path)
                    .await?;
                vec![(counterparty.clone(), receiver_path.clone())]
            }
            (Some(counterparty), None) => {
                let counterparties = self
                    .payment_request_counterparties_for_counterparty(
                        counterparty,
                        filter.received_only,
                    )
                    .await?;
                for (_, receiver_path) in &counterparties {
                    self.ensure_peer_not_blocked(counterparty, receiver_path)
                        .await?;
                }
                counterparties
            }
            _ => {
                self.payment_request_counterparties(filter.received_only)
                    .await?
            }
        };

        let mut records = Vec::new();
        for (counterparty, counterparty_receiver_path) in counterparties {
            let mut peer_records = if filter.received_only {
                derive_received_payment_request_records(
                    &self.storage,
                    &counterparty,
                    &counterparty_receiver_path,
                    now,
                )
                .await?
            } else {
                derive_payment_request_records(
                    &self.storage,
                    &counterparty,
                    &counterparty_receiver_path,
                    now,
                )
                .await?
            };
            self.mark_recovery_required_payment_request_records(
                &counterparty,
                &counterparty_receiver_path,
                &mut peer_records,
            )
            .await?;
            records.extend(
                peer_records
                    .into_iter()
                    .filter(|record| filter.matches(record)),
            );
        }
        sort_payment_requests_newest_first(&mut records);
        Ok(records)
    }

    /// Return all Payment Requests across non-blocked counterparties.
    pub async fn payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        self.list_payment_requests(PaymentRequestFilter::default())
            .await
    }

    /// Return accepted recurring Payment Requests across non-blocked counterparties.
    pub async fn active_recurring_payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        self.list_payment_requests(PaymentRequestFilter {
            states: vec![PaymentRequestLifecycleState::ActiveRecurring],
            recurring: Some(true),
            ..PaymentRequestFilter::default()
        })
        .await
    }

    /// Return received Payment Requests that need a local payer response.
    pub async fn actionable_received_payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        self.list_payment_requests(PaymentRequestFilter {
            local_role: Some(PaymentRequestLocalRole::Payer),
            states: vec![
                PaymentRequestLifecycleState::Proposed,
                PaymentRequestLifecycleState::ProposalExpired,
            ],
            ..PaymentRequestFilter::default()
        })
        .await
    }

    async fn payment_request_counterparties(
        &self,
        received_only: bool,
    ) -> Result<Vec<(PubkyPublicKey, PaykitReceiverPath)>> {
        self.storage
            .transaction(move |tx| {
                let snapshot = tx.export_storage_state();
                let mut counterparties = HashSet::new();
                for item in snapshot.private_stream_items {
                    if is_payment_request_kind(item.parsed_kind.as_deref()) {
                        counterparties.insert((item.counterparty, item.counterparty_receiver_path));
                    }
                }
                if !received_only {
                    for outbound in snapshot.outbound_private_messages {
                        if is_payment_request_kind(Some(&outbound.kind)) {
                            counterparties.insert((
                                outbound.counterparty,
                                outbound.counterparty_receiver_path,
                            ));
                        }
                    }
                }
                let mut counterparties = counterparties
                    .into_iter()
                    .filter(|(counterparty, receiver_path)| {
                        !snapshot
                            .linked_peers
                            .get(&(counterparty.clone(), receiver_path.clone()))
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .collect::<Vec<_>>();
                counterparties.sort_by(|(left_key, left_receiver), (right_key, right_receiver)| {
                    left_key
                        .as_str()
                        .cmp(right_key.as_str())
                        .then_with(|| left_receiver.as_str().cmp(right_receiver.as_str()))
                });
                Ok(counterparties)
            })
            .await
    }

    async fn payment_request_counterparties_for_counterparty(
        &self,
        counterparty: &PubkyPublicKey,
        received_only: bool,
    ) -> Result<Vec<(PubkyPublicKey, PaykitReceiverPath)>> {
        self.storage
            .transaction(move |tx| {
                let snapshot = tx.export_storage_state();
                let mut receiver_paths = HashSet::new();
                for item in snapshot.private_stream_items {
                    if &item.counterparty == counterparty
                        && is_payment_request_kind(item.parsed_kind.as_deref())
                    {
                        receiver_paths.insert(item.counterparty_receiver_path);
                    }
                }
                if !received_only {
                    for outbound in snapshot.outbound_private_messages {
                        if &outbound.counterparty == counterparty
                            && is_payment_request_kind(Some(&outbound.kind))
                        {
                            receiver_paths.insert(outbound.counterparty_receiver_path);
                        }
                    }
                }
                let mut counterparties = receiver_paths
                    .into_iter()
                    .map(|receiver_path| (counterparty.clone(), receiver_path))
                    .collect::<Vec<_>>();
                counterparties.sort_by(|(_, left_receiver), (_, right_receiver)| {
                    left_receiver.as_str().cmp(right_receiver.as_str())
                });
                Ok(counterparties)
            })
            .await
    }

    pub(super) async fn ensure_private_outbound_ready(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<()> {
        self.require_identity_and_session().await?;
        self.ensure_peer_allows_private_automation(counterparty, counterparty_receiver_path)
            .await?;

        let has_active_link = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .encrypted_link_state(counterparty, counterparty_receiver_path)
                    .and_then(|state| state.link_snapshot)
                    .is_some())
            })
            .await?;
        if !has_active_link {
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no active Encrypted Link snapshot for counterparty {counterparty}"
                ),
                source: None,
            });
        }

        Ok(())
    }

    /// Queue a new Payment Request proposal and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn propose_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        terms: PaymentRequestTerms,
    ) -> Result<PaymentRequestRecord> {
        let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);
        let payment_request_id = event.payment_request_id().clone();
        self.enqueue_raw_payment_request(
            counterparty.clone(),
            counterparty_receiver_path.clone(),
            &event,
        )
        .await?;
        self.load_payment_request_record(
            &counterparty,
            &counterparty_receiver_path,
            &payment_request_id,
        )
        .await
    }

    /// Queue acceptance for a received Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn accept_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let (_, expected_identity) = self.load_session_access_and_refresh_identity().await?;
        let record = self
            .load_payment_request_record(
                &counterparty,
                &counterparty_receiver_path,
                payment_request_id,
            )
            .await?;
        require_payer_role(&record, "accept Payment Request")?;
        require_state(
            &record,
            &[PaymentRequestLifecycleState::Proposed],
            "accept Payment Request",
        )?;
        let event = PaymentRequestAcceptance::new(EventId::new_v4(), payment_request_id.clone());
        self.enqueue_manual_payment_request_response(
            counterparty.clone(),
            counterparty_receiver_path.clone(),
            paykit_lib::PaymentRequestEvent::Acceptance(event),
            Some(expected_identity),
        )
        .await?;
        self.load_payment_request_record(
            &counterparty,
            &counterparty_receiver_path,
            payment_request_id,
        )
        .await
    }

    /// Queue rejection for a received Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn reject_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord> {
        let (_, expected_identity) = self.load_session_access_and_refresh_identity().await?;
        let record = self
            .load_payment_request_record(
                &counterparty,
                &counterparty_receiver_path,
                payment_request_id,
            )
            .await?;
        require_payer_role(&record, "reject Payment Request")?;
        require_state(
            &record,
            &[
                PaymentRequestLifecycleState::Proposed,
                PaymentRequestLifecycleState::ProposalExpired,
            ],
            "reject Payment Request",
        )?;
        let event =
            PaymentRequestRejection::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_manual_payment_request_response(
            counterparty.clone(),
            counterparty_receiver_path.clone(),
            paykit_lib::PaymentRequestEvent::Rejection(event),
            Some(expected_identity),
        )
        .await?;
        self.load_payment_request_record(
            &counterparty,
            &counterparty_receiver_path,
            payment_request_id,
        )
        .await
    }

    /// Queue cancellation for a known non-terminal Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn cancel_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord> {
        let (_, expected_identity) = self.load_session_access_and_refresh_identity().await?;
        let record = self
            .load_payment_request_record(
                &counterparty,
                &counterparty_receiver_path,
                payment_request_id,
            )
            .await?;
        require_state(
            &record,
            &[
                PaymentRequestLifecycleState::Proposed,
                PaymentRequestLifecycleState::ProposalExpired,
                PaymentRequestLifecycleState::Accepted,
                PaymentRequestLifecycleState::ActiveRecurring,
                PaymentRequestLifecycleState::ProofSubmitted,
            ],
            "cancel Payment Request",
        )?;
        let event =
            PaymentRequestCancellation::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_manual_payment_request_response(
            counterparty.clone(),
            counterparty_receiver_path.clone(),
            paykit_lib::PaymentRequestEvent::Cancellation(event),
            Some(expected_identity),
        )
        .await?;
        self.load_payment_request_record(
            &counterparty,
            &counterparty_receiver_path,
            payment_request_id,
        )
        .await
    }

    /// Queue a Payment Proof for an accepted Payment Request and return local derived state.
    ///
    /// A canceled request remains eligible only when its record retains a valid
    /// Acceptance. In that case, the caller must have durably recorded in its
    /// own wallet state that payment execution passed its irreversible boundary
    /// before observing cancellation. Paykit cannot derive that fact from events.
    /// This is a caller-enforced precondition; the SDK does not inspect or verify
    /// the wallet's durable execution evidence.
    /// Queueing the proof does not execute a payment or reopen the request.
    /// Use [`Self::submit_payment_proof_submission`] to report Allowance attribution.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn submit_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        payment_request_id: &PaymentRequestId,
        billing_period: Option<BillingPeriod>,
        payment_endpoint_identifier: PaymentEndpointIdentifier,
        proof: JsonMap<String, JsonValue>,
    ) -> Result<PaymentRequestRecord> {
        self.submit_payment_proof_submission(
            counterparty,
            counterparty_receiver_path,
            payment_request_id,
            PaymentProofSubmission {
                billing_period,
                payment_endpoint_identifier,
                proof,
                allowance_id: None,
            },
        )
        .await
    }

    /// Queue payment evidence with optional informational Allowance attribution.
    ///
    /// The caller supplies attribution from its durable payment association.
    /// An unknown, ended, expired, or replaced Allowance may describe a historical
    /// payment; the SDK does not infer authority or update usage from this claim.
    /// Repeated or corrective proofs preserve evidence for the same occurrence
    /// and do not imply another payment or settlement confirmation.
    ///
    /// A canceled request requires a recorded Acceptance and caller evidence
    /// that execution crossed its irreversible boundary before cancellation was
    /// observed. This is a caller-enforced precondition; the SDK does not inspect
    /// or verify the wallet's durable execution evidence. Queuing evidence never
    /// reopens the request. Session creation, capability scope, key rotation, and
    /// settlement validation remain caller responsibilities. The returned record
    /// describes the local outbound queue.
    pub async fn submit_payment_proof_submission(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        payment_request_id: &PaymentRequestId,
        submission: PaymentProofSubmission,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(
                &counterparty,
                &counterparty_receiver_path,
                payment_request_id,
            )
            .await?;
        require_payer_role(&record, "submit Payment Proof")?;
        require_state(
            &record,
            payment_proof_allowed_states(&record),
            "submit Payment Proof",
        )?;
        let request = request_from_record(&record).ok_or_else(|| PaykitSdkError::Protocol {
            context: "Payment Request terms are unavailable".into(),
            source: None,
        })?;
        let mut event = PaymentProof::new(
            EventId::new_v4(),
            payment_request_id.clone(),
            request.request().payment_reference().clone(),
            submission.billing_period,
            submission.payment_endpoint_identifier,
            submission.proof,
        );
        if let Some(allowance_id) = submission.allowance_id {
            event = event.with_allowance_id(allowance_id);
        }
        event.validate_for_request(&request)?;
        self.enqueue_raw_payment_proof(
            counterparty.clone(),
            counterparty_receiver_path.clone(),
            &event,
        )
        .await?;
        self.load_payment_request_record(
            &counterparty,
            &counterparty_receiver_path,
            payment_request_id,
        )
        .await
    }

    async fn load_payment_request_record(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let mut records = derive_payment_request_records(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            self.clock.now(),
        )
        .await?;
        self.mark_recovery_required_payment_request_records(
            counterparty,
            counterparty_receiver_path,
            &mut records,
        )
        .await?;
        records
            .into_iter()
            .find(|record| record.payment_request_id == payment_request_id.as_str())
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "Payment Request {} is not known for counterparty {}",
                    payment_request_id, counterparty
                ),
                source: None,
            })
    }

    async fn mark_recovery_required_payment_request_records(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        records: &mut [PaymentRequestRecord],
    ) -> Result<()> {
        let recovery_required = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .linked_peer(counterparty, counterparty_receiver_path)
                    .is_some_and(|peer| peer.state == LinkedPeerState::RecoveryRequired))
            })
            .await?;
        if !recovery_required {
            return Ok(());
        }
        for record in records {
            if matches!(
                record.state,
                PaymentRequestLifecycleState::Proposed
                    | PaymentRequestLifecycleState::ProposalExpired
                    | PaymentRequestLifecycleState::Accepted
                    | PaymentRequestLifecycleState::ProofSubmitted
                    | PaymentRequestLifecycleState::ActiveRecurring
            ) {
                record.state = PaymentRequestLifecycleState::RecoveryRequired;
            }
        }
        Ok(())
    }

    pub(crate) async fn enqueue_raw_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        event: &PaymentRequest,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(&counterparty, &counterparty_receiver_path)
            .await?;
        enqueue_payment_request_message(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            event,
            self.clock.now(),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn enqueue_raw_payment_request_acceptance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        event: &PaymentRequestAcceptance,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.enqueue_manual_payment_request_response(
            counterparty,
            counterparty_receiver_path,
            paykit_lib::PaymentRequestEvent::Acceptance(event.clone()),
            None,
        )
        .await
    }

    async fn enqueue_manual_payment_request_response(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        event: paykit_lib::PaymentRequestEvent,
        expected_identity: Option<IdentityState>,
    ) -> Result<OutboundPrivateMessageRecord> {
        let (_, current_identity) = self.load_session_access_and_refresh_identity().await?;
        let validate_current_state = expected_identity.is_some();
        let identity = expected_identity.unwrap_or(current_identity);
        self.ensure_private_outbound_ready(&counterparty, &counterparty_receiver_path)
            .await?;
        let local = self.config.receiver_path.clone();
        let now = self.clock.now();
        let raw = paykit_lib::serialize_payment_request_event(&event)?;
        let kind = crate::domain::outbound_private::validate_outbound_private_message(&raw)?;
        self.storage
            .transaction(move |tx| {
                super::allowance_accounting::ensure_accounting_identity(tx, &identity)?;
                if validate_current_state {
                    let record =
                        crate::domain::payment_requests::payment_request_records_in_transaction(
                            tx,
                            &counterparty,
                            &counterparty_receiver_path,
                            now,
                        )?
                        .into_iter()
                        .find(|r| r.payment_request_id == event.payment_request_id().as_str())
                        .ok_or_else(|| PaykitSdkError::Policy {
                            context: "Payment Request changed before manual response".into(),
                            source: None,
                        })?;
                    use PaymentRequestLifecycleState as State;
                    match &event {
                        paykit_lib::PaymentRequestEvent::Acceptance(_) => {
                            require_payer_role(&record, "accept Payment Request")?;
                            require_state(&record, &[State::Proposed], "accept Payment Request")?;
                        }
                        paykit_lib::PaymentRequestEvent::Rejection(_) => {
                            require_payer_role(&record, "reject Payment Request")?;
                            require_state(
                                &record,
                                &[State::Proposed, State::ProposalExpired],
                                "reject Payment Request",
                            )?;
                        }
                        paykit_lib::PaymentRequestEvent::Cancellation(_) => require_state(
                            &record,
                            &[
                                State::Proposed,
                                State::ProposalExpired,
                                State::Accepted,
                                State::ActiveRecurring,
                                State::ProofSubmitted,
                            ],
                            "cancel Payment Request",
                        )?,
                        _ => unreachable!("Private helper only receives manual response events"),
                    }
                }
                crate::domain::allowance_accounting::manual_response(
                    tx,
                    &local,
                    crate::PaymentRequestScope {
                        counterparty: counterparty.clone(),
                        counterparty_receiver_path: counterparty_receiver_path.clone(),
                        payment_request_id: event.payment_request_id().clone(),
                    },
                )?;
                Ok(tx.insert_outbound_private_message(
                    crate::storage::NewOutboundPrivateMessage::new(
                        counterparty,
                        counterparty_receiver_path,
                        kind,
                        raw,
                        now,
                    ),
                ))
            })
            .await
    }

    pub(crate) async fn enqueue_raw_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        event: &PaymentProof,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(&counterparty, &counterparty_receiver_path)
            .await?;
        enqueue_payment_proof_message(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            event,
            self.clock.now(),
        )
        .await
    }
}

fn require_payer_role(record: &PaymentRequestRecord, action: &str) -> Result<()> {
    if record.local_role == Some(PaymentRequestLocalRole::Payer) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: local identity is not the payer"),
            source: None,
        })
    }
}

fn require_state(
    record: &PaymentRequestRecord,
    allowed: &[PaymentRequestLifecycleState],
    action: &str,
) -> Result<()> {
    if allowed.contains(&record.state) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: Payment Request {} is in state {:?}",
                record.payment_request_id, record.state
            ),
            source: None,
        })
    }
}

fn is_payment_request_kind(kind: Option<&str>) -> bool {
    matches!(
        kind.and_then(PrivateMessageKind::parse),
        Some(
            PrivateMessageKind::PaymentRequest
                | PrivateMessageKind::PaymentRequestAcceptance
                | PrivateMessageKind::PaymentRequestRejection
                | PrivateMessageKind::PaymentRequestCancellation
                | PrivateMessageKind::PaymentProof
        )
    )
}

fn sort_payment_requests_newest_first(records: &mut [PaymentRequestRecord]) {
    records.sort_by(|left, right| {
        right
            .last_event_at
            .cmp(&left.last_event_at)
            .then_with(|| right.last_stream_item_id.cmp(&left.last_stream_item_id))
            .then_with(|| {
                right
                    .last_outbound_message_id
                    .cmp(&left.last_outbound_message_id)
            })
            .then_with(|| left.counterparty.as_str().cmp(right.counterparty.as_str()))
            .then_with(|| left.payment_request_id.cmp(&right.payment_request_id))
    });
}
