use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Return received Payment Request records for one counterparty.
    ///
    /// Records are derived from the persisted inbound private stream and
    /// returned newest-first by last applied stream item. Malformed recognized
    /// Payment Request events without a valid `payment_request_id` stay in the
    /// raw private stream log and cannot be attached to a request-scoped record.
    pub async fn received_payment_request_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PaymentRequestRecord>> {
        self.ensure_private_workflows_enabled("Payment Request record access")?;
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        derive_received_payment_request_records(&self.storage, counterparty, self.clock.now()).await
    }

    /// Return merged local Payment Request records for one counterparty.
    ///
    /// Records combine received private-stream events and local outbound
    /// Payment Request events, returned newest-first.
    pub async fn payment_request_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PaymentRequestRecord>> {
        self.ensure_private_workflows_enabled("Payment Request record access")?;
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        derive_payment_request_records(&self.storage, counterparty, self.clock.now()).await
    }

    pub(super) async fn ensure_private_outbound_ready(
        &self,
        counterparty: &PubkyPublicKey,
        disabled_message: &str,
    ) -> Result<()> {
        self.ensure_private_workflows_enabled(disabled_message)?;

        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Err(PaykitSdkError::Identity {
                context: "local Pubky identity is not private-link-capable".into(),
                source: None,
            });
        }
        if session_access.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "no Pubky session available".into(),
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
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
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
        let event = PaymentRequestAcceptance::new(EventId::new_v4(), payment_request_id.clone());
        self.enqueue_raw_payment_request_acceptance(counterparty.clone(), &event)
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
            &[PaymentRequestLifecycleState::Proposed],
            "reject Payment Request",
        )?;
        let event =
            PaymentRequestRejection::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_raw_payment_request_rejection(counterparty.clone(), &event)
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
        let request = request_from_record(&record).ok_or_else(|| {
            PaykitSdkError::Protocol("Payment Request terms are unavailable".into())
        })?;
        let event = PaymentProof::new(
            EventId::new_v4(),
            payment_request_id.clone(),
            request.request.payment_reference.clone(),
            billing_period,
            payment_endpoint_identifier,
            proof,
        );
        event.validate_for_request(&request)?;
        self.enqueue_raw_payment_proof(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    async fn load_payment_request_record(
        &self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        derive_payment_request_records(&self.storage, counterparty, self.clock.now())
            .await?
            .into_iter()
            .find(|record| record.payment_request_id == payment_request_id.as_str())
            .ok_or_else(|| {
                PaykitSdkError::Protocol(format!(
                    "Payment Request {} is not known for counterparty {}",
                    payment_request_id, counterparty
                ))
            })
    }

    /// Enqueue one raw Payment Request protocol event for outbound delivery.
    ///
    /// This validates private-send readiness and stores canonical JSON, but it
    /// does not enforce role, lifecycle, or proof/request correlation policy.
    pub async fn enqueue_raw_payment_request_event(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestEvent,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_event_message(&self.storage, counterparty, event, self.clock.now())
            .await
    }

    /// Enqueue a raw Payment Request proposal for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequest,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_message(&self.storage, counterparty, event, self.clock.now()).await
    }

    /// Enqueue a raw Payment Request acceptance for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request_acceptance(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestAcceptance,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_acceptance_message(
            &self.storage,
            counterparty,
            event,
            self.clock.now(),
        )
        .await
    }

    /// Enqueue a raw Payment Request rejection for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request_rejection(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestRejection,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_rejection_message(
            &self.storage,
            counterparty,
            event,
            self.clock.now(),
        )
        .await
    }

    /// Enqueue a raw Payment Request cancellation for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request_cancellation(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestCancellation,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_cancellation_message(
            &self.storage,
            counterparty,
            event,
            self.clock.now(),
        )
        .await
    }

    /// Enqueue a raw Payment Proof for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role, lifecycle, or
    /// proof/request correlation policy.
    pub async fn enqueue_raw_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentProof,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_proof_message(&self.storage, counterparty, event, self.clock.now()).await
    }
}

fn require_payer_role(record: &PaymentRequestRecord, action: &str) -> Result<()> {
    if record.local_role == Some(PaymentRequestLocalRole::Payer) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy(format!(
            "cannot {action}: local identity is not the payer"
        )))
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
        Err(PaykitSdkError::Policy(format!(
            "cannot {action}: Payment Request {} is in state {:?}",
            record.payment_request_id, record.state
        )))
    }
}
