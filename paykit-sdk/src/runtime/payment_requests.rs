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
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        let mut records =
            derive_received_payment_request_records(&self.storage, counterparty, self.clock.now())
                .await?;
        self.mark_recovery_required_payment_request_records(counterparty, &mut records)
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
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        let mut records =
            derive_payment_request_records(&self.storage, counterparty, self.clock.now()).await?;
        self.mark_recovery_required_payment_request_records(counterparty, &mut records)
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
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        let now = self.clock.now();

        let counterparties = if let Some(counterparty) = &filter.counterparty {
            self.ensure_peer_not_blocked(counterparty).await?;
            vec![counterparty.clone()]
        } else {
            self.payment_request_counterparties(filter.received_only)
                .await?
        };

        let mut records = Vec::new();
        for counterparty in counterparties {
            let mut peer_records = if filter.received_only {
                derive_received_payment_request_records(&self.storage, &counterparty, now).await?
            } else {
                derive_payment_request_records(&self.storage, &counterparty, now).await?
            };
            self.mark_recovery_required_payment_request_records(&counterparty, &mut peer_records)
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

    /// Return accepted recurring Payment Requests from currently authorized remote apps.
    pub async fn active_recurring_payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        let records = self
            .list_payment_requests(PaymentRequestFilter {
                states: vec![PaymentRequestLifecycleState::ActiveRecurring],
                recurring: Some(true),
                ..PaymentRequestFilter::default()
            })
            .await?;
        self.filter_authorized_remote_payment_request_apps(records)
            .await
    }

    /// Return received Payment Requests from currently authorized apps that need a response.
    pub async fn actionable_received_payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        let records = self
            .list_payment_requests(PaymentRequestFilter {
                local_role: Some(PaymentRequestLocalRole::Payer),
                states: vec![
                    PaymentRequestLifecycleState::Proposed,
                    PaymentRequestLifecycleState::ProposalExpired,
                ],
                ..PaymentRequestFilter::default()
            })
            .await?;
        self.filter_authorized_remote_payment_request_apps(records)
            .await
    }

    async fn filter_authorized_remote_payment_request_apps(
        &self,
        records: Vec<PaymentRequestRecord>,
    ) -> Result<Vec<PaymentRequestRecord>> {
        let counterparties = records
            .iter()
            .filter(|record| remote_payment_request_app(record).is_some())
            .map(|record| record.counterparty.clone())
            .collect::<HashSet<_>>();
        let mut authorized_by_counterparty = HashMap::new();
        for counterparty in counterparties {
            authorized_by_counterparty.insert(
                counterparty.clone(),
                self.authorized_payment_request_apps_for_peer(&counterparty)
                    .await?,
            );
        }
        Ok(records
            .into_iter()
            .filter(|record| {
                let Some(app_id) = remote_payment_request_app(record) else {
                    return record.local_role == Some(PaymentRequestLocalRole::Payee);
                };
                authorized_by_counterparty
                    .get(&record.counterparty)
                    .and_then(Option::as_ref)
                    .is_some_and(|app_ids| app_ids.contains(app_id))
            })
            .collect())
    }

    pub(super) async fn ensure_payment_request_origin_app_authorized(
        &self,
        counterparty: &PubkyPublicKey,
        record: &PaymentRequestRecord,
        action: &str,
    ) -> Result<()> {
        let proposal_app_id =
            record
                .proposal_app_id
                .as_ref()
                .ok_or_else(|| PaykitSdkError::Protocol {
                    context: format!(
                        "cannot {action}: Payment Request {} has no originating Paykit App",
                        record.payment_request_id
                    ),
                    source: None,
                })?;
        if self
            .authorized_payment_request_apps_for_peer(counterparty)
            .await?
            .is_some_and(|app_ids| app_ids.contains(proposal_app_id))
        {
            return Ok(());
        }
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: originating Paykit app '{}' is not currently authorized for Payment Requests",
                proposal_app_id
            ),
            source: None,
        })
    }

    pub(super) async fn authorized_payment_request_apps_for_peer(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<Vec<paykit_lib::PaykitAppId>>> {
        let context = self
            .counterparty_app_authorization_context(counterparty)
            .await?;
        Ok(context.payment_request_apps)
    }

    async fn payment_request_counterparties(
        &self,
        received_only: bool,
    ) -> Result<Vec<PubkyPublicKey>> {
        self.storage
            .transaction(move |tx| {
                let snapshot = tx.export_storage_state();
                let mut counterparties = HashSet::new();
                for item in snapshot.private_stream_items {
                    if is_payment_request_kind(item.parsed_kind.as_deref()) {
                        counterparties.insert(item.counterparty);
                    }
                }
                if !received_only {
                    for outbound in snapshot.outbound_private_messages {
                        if is_payment_request_kind(Some(&outbound.kind)) {
                            counterparties.insert(outbound.counterparty);
                        }
                    }
                }
                let mut counterparties = counterparties
                    .into_iter()
                    .filter(|counterparty| {
                        !snapshot
                            .linked_peers
                            .get(counterparty)
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .collect::<Vec<_>>();
                counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                Ok(counterparties)
            })
            .await
    }

    pub(super) async fn ensure_private_outbound_ready(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        if !session_access.private_link_capable_for_capabilities(PAYKIT_SESSION_CAPABILITIES)? {
            return Err(PaykitSdkError::Identity {
                context: "local Pubky identity is not private-link-capable".into(),
                source: None,
            });
        }
        self.ensure_peer_allows_private_automation(counterparty)
            .await?;

        let has_active_link = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .encrypted_link_state(counterparty)
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
        terms: PaymentRequestTerms,
    ) -> Result<PaymentRequestRecord> {
        let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);
        let payment_request_id = event.payment_request_id.clone();
        self.enqueue_raw_payment_request(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, &payment_request_id)
            .await
    }

    /// Queue acceptance for a received Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn accept_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_payer_role(&record, "accept Payment Request")?;
        require_state(
            &record,
            &[PaymentRequestLifecycleState::Proposed],
            "accept Payment Request",
        )?;
        self.ensure_payment_request_origin_app_authorized(
            &counterparty,
            &record,
            "accept Payment Request",
        )
        .await?;
        let event = PaymentRequestAcceptance::new(EventId::new_v4(), payment_request_id.clone());
        self.enqueue_raw_payment_request_response(
            counterparty.clone(),
            &PaymentRequestEvent::Acceptance(event),
        )
        .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Queue rejection for a received Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn reject_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
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
        self.ensure_payment_request_origin_app_authorized(
            &counterparty,
            &record,
            "reject Payment Request",
        )
        .await?;
        let event =
            PaymentRequestRejection::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_raw_payment_request_response(
            counterparty.clone(),
            &PaymentRequestEvent::Rejection(event),
        )
        .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Queue cancellation for a known non-terminal Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn cancel_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
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
        if record.local_role == Some(PaymentRequestLocalRole::Payer) {
            self.ensure_payment_request_origin_app_authorized(
                &counterparty,
                &record,
                "cancel Payment Request",
            )
            .await?;
        }
        require_payment_request_action_app(&record, &self.config.app_id, "cancel Payment Request")?;
        let event =
            PaymentRequestCancellation::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_raw_payment_request_cancellation(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Queue a Payment Proof for an accepted Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn submit_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        billing_period: Option<BillingPeriod>,
        payment_app_id: paykit_lib::PaykitAppId,
        payment_endpoint_identifier: PaymentEndpointIdentifier,
        proof: JsonMap<String, JsonValue>,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_payer_role(&record, "submit Payment Proof")?;
        require_state(
            &record,
            &[
                PaymentRequestLifecycleState::Accepted,
                PaymentRequestLifecycleState::ActiveRecurring,
            ],
            "submit Payment Proof",
        )?;
        require_payer_app(&record, &self.config.app_id, "submit Payment Proof")?;
        self.ensure_payment_request_origin_app_authorized(
            &counterparty,
            &record,
            "submit Payment Proof",
        )
        .await?;
        let request = request_from_record(&record).ok_or_else(|| PaykitSdkError::Protocol {
            context: "Payment Request terms are unavailable".into(),
            source: None,
        })?;
        let event = PaymentProof::new(
            EventId::new_v4(),
            payment_request_id.clone(),
            request.request.payment_reference.clone(),
            billing_period,
            payment_app_id,
            payment_endpoint_identifier,
            proof,
        );
        event.validate_for_request(&request)?;
        self.enqueue_raw_payment_proof(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    pub(super) async fn load_payment_request_record(
        &self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let mut records =
            derive_payment_request_records(&self.storage, counterparty, self.clock.now()).await?;
        self.mark_recovery_required_payment_request_records(counterparty, &mut records)
            .await?;
        records
            .into_iter()
            .find(|record| record.payment_request_id == payment_request_id.as_str())
            .ok_or_else(|| PaykitSdkError::NotFound {
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
        records: &mut [PaymentRequestRecord],
    ) -> Result<()> {
        let recovery_required = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .linked_peer(counterparty)
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
        event: &PaymentRequest,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(&counterparty).await?;
        enqueue_payment_request_message(
            &self.storage,
            counterparty,
            &self.config.app_id,
            event,
            self.clock.now(),
        )
        .await
    }

    async fn enqueue_raw_payment_request_response(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestEvent,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(&counterparty).await?;
        enqueue_checked_payment_request_action(
            &self.storage,
            counterparty,
            &self.config.app_id,
            event,
            self.clock.now(),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn enqueue_raw_payment_request_acceptance(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestAcceptance,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.enqueue_raw_payment_request_response(
            counterparty,
            &PaymentRequestEvent::Acceptance(event.clone()),
        )
        .await
    }

    pub(crate) async fn enqueue_raw_payment_request_cancellation(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestCancellation,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(&counterparty).await?;
        enqueue_checked_payment_request_action(
            &self.storage,
            counterparty,
            &self.config.app_id,
            &PaymentRequestEvent::Cancellation(event.clone()),
            self.clock.now(),
        )
        .await
    }

    pub(crate) async fn enqueue_raw_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentProof,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(&counterparty).await?;
        enqueue_checked_payment_request_action(
            &self.storage,
            counterparty,
            &self.config.app_id,
            &PaymentRequestEvent::Proof(event.clone()),
            self.clock.now(),
        )
        .await
    }
}

fn remote_payment_request_app(record: &PaymentRequestRecord) -> Option<&paykit_lib::PaykitAppId> {
    match record.local_role {
        Some(PaymentRequestLocalRole::Payer) => record.proposal_app_id.as_ref(),
        Some(PaymentRequestLocalRole::Payee) => record.payer_app_id.as_ref(),
        None => None,
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

fn require_payer_app(
    record: &PaymentRequestRecord,
    app_id: &paykit_lib::PaykitAppId,
    action: &str,
) -> Result<()> {
    if record.payer_app_id.as_ref() == Some(app_id) {
        return Ok(());
    }
    Err(PaykitSdkError::Policy {
        context: format!("cannot {action}: another Paykit app owns the payer response"),
        source: None,
    })
}

fn require_payment_request_action_app(
    record: &PaymentRequestRecord,
    app_id: &paykit_lib::PaykitAppId,
    action: &str,
) -> Result<()> {
    match record.local_role {
        Some(PaymentRequestLocalRole::Payee) => {
            if record.proposal_app_id.as_ref() == Some(app_id) {
                Ok(())
            } else {
                Err(PaykitSdkError::Policy {
                    context: format!("cannot {action}: another Paykit app created the request"),
                    source: None,
                })
            }
        }
        Some(PaymentRequestLocalRole::Payer) if record.payer_app_id.is_some() => {
            require_payer_app(record, app_id, action)
        }
        Some(PaymentRequestLocalRole::Payer) => Ok(()),
        None => Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: local Payment Request role is unknown"),
            source: None,
        }),
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
