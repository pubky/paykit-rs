use super::*;
use crate::domain::payment_requests::{
    payment_request_records_from_transaction, received_payment_request_records_from_transaction,
    validate_proof_conversion,
};
use crate::domain::private_stream::is_payment_request_kind;
use paykit_lib::{ConversionRate, PaymentConversionQuote, PaymentRequestEvent};

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
    /// inbound events with local outbound context. Quotes are omitted because
    /// admission depends on the outbound acceptance history. Malformed recognized
    /// Payment Request events without a valid `payment_request_id` stay in the raw
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
        let (_, identity, records) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                filtered_payment_request_records(tx, &filter, self.clock.now())
            })
            .await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        if let Some(records) = records {
            return Ok(records);
        }
        self.storage
            .transaction(|tx| filtered_payment_request_records(tx, &filter, self.clock.now()))
            .await
    }

    /// Return all Payment Requests across non-blocked counterparties.
    pub async fn payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        self.list_payment_requests(PaymentRequestFilter::default())
            .await
    }

    /// Return accepted recurring Payment Requests from currently authorized remote apps.
    /// Counterparties whose registry cannot be fetched or validated are omitted.
    pub async fn active_recurring_payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        let records = self
            .list_payment_requests(PaymentRequestFilter {
                states: vec![PaymentRequestLifecycleState::ActiveRecurring],
                recurring: Some(true),
                ..PaymentRequestFilter::default()
            })
            .await?;
        Ok(self
            .filter_authorized_remote_payment_request_apps(records)
            .await?
            .into_iter()
            .filter(|record| self.execution_available_to_current_app(record))
            .collect())
    }

    /// Return received Payment Requests from currently authorized apps that need a response.
    /// Counterparties whose registry cannot be fetched or validated are omitted.
    pub async fn actionable_received_payment_requests(&self) -> Result<Vec<PaymentRequestRecord>> {
        let records = self
            .list_payment_requests(PaymentRequestFilter {
                local_role: Some(PaymentRequestLocalRole::Payer),
                states: vec![
                    PaymentRequestLifecycleState::Proposed,
                    PaymentRequestLifecycleState::ProposalExpired,
                    PaymentRequestLifecycleState::Accepted,
                    PaymentRequestLifecycleState::ActiveRecurring,
                ],
                ..PaymentRequestFilter::default()
            })
            .await?;
        Ok(self
            .filter_authorized_remote_payment_request_apps(records)
            .await?
            .into_iter()
            .filter(|record| self.execution_available_to_current_app(record))
            .collect())
    }

    fn execution_available_to_current_app(&self, record: &PaymentRequestRecord) -> bool {
        record
            .execution_claim_app_id
            .as_ref()
            .is_none_or(|app_id| app_id == &self.config.app_id)
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
            let authorized = match self
                .authorized_payment_request_apps_for_peer(&counterparty)
                .await
            {
                Ok(authorized) => authorized,
                Err(PaykitSdkError::Transport { .. } | PaykitSdkError::Protocol { .. }) => None,
                Err(err) => return Err(err),
            };
            authorized_by_counterparty.insert(counterparty, authorized);
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

    pub(super) async fn ensure_private_outbound_ready(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<GuardedSessionAccess> {
        let (session_access, _, readiness) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                Ok(require_private_automation_ready(
                    tx.linked_peer(counterparty).map(|peer| peer.state),
                    tx.encrypted_link_state(counterparty)
                        .is_some_and(|state| state.link_snapshot.is_some()),
                    counterparty,
                ))
            })
            .await?;
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
        readiness.expect("active session loads outbound readiness")?;

        Ok(session_access)
    }

    /// Queue a new Payment Request proposal and return local derived state.
    ///
    /// Before using conversion or payment deadlines, the caller must establish
    /// that the counterparty supports those extensions; they are not negotiated here.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing. It is derived in the queue transaction, without
    /// a fallible post-commit read.
    pub async fn propose_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        terms: PaymentRequestTerms,
    ) -> Result<PaymentRequestRecord> {
        let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);
        self.enqueue_raw_payment_request(counterparty, &event).await
    }

    /// Claim a received Payment Request before beginning payment preparation.
    ///
    /// The shared claim hides the work from other local Paykit Apps. It remains
    /// through acceptance and may be explicitly released while payment remains
    /// unresolved. Payment execution must wait for
    /// [`Self::accept_payment_request`] to succeed when the request is still
    /// proposed.
    pub async fn claim_payment_request_for_execution(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_role(
            &record,
            PaymentRequestLocalRole::Payer,
            "claim Payment Request for execution",
        )?;
        self.ensure_payment_request_origin_app_authorized(
            &counterparty,
            &record,
            "claim Payment Request for execution",
        )
        .await?;
        claim_payment_request_execution(
            &self.storage,
            counterparty,
            &self.config.app_id,
            payment_request_id,
            self.clock.now(),
        )
        .await
    }

    /// Release this App's unresolved Payment Request execution claim.
    ///
    /// Releasing does not reject the request and makes it actionable to other
    /// compatible local Paykit Apps again. The request's endpoint constraints
    /// remain unchanged. One-time requests cannot be released after proof;
    /// recurring requests retain completed billing-period proofs. Prepared,
    /// Submitted, or Unknown accounting attempts must be reconciled before release.
    pub async fn release_payment_request_execution_claim(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        release_payment_request_execution_claim(
            &self.storage,
            counterparty,
            &self.config.app_id,
            payment_request_id,
            self.clock.now(),
        )
        .await
    }

    /// Queue acceptance for a claimed received Payment Request.
    ///
    /// The current App must claim the request first. Success only queues acceptance;
    /// accounting callers must still reserve payment and obtain a fresh handoff.
    /// The execution claim remains until proof, cancellation, or an explicit release,
    /// and longer while payment accounting has unresolved attempts or a canceled
    /// request has successful payments awaiting proof.
    /// The returned record does not imply delivery or counterparty processing.
    pub async fn accept_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let (_, expected_identity) = self.load_session_access_and_refresh_identity().await?;
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_role(
            &record,
            PaymentRequestLocalRole::Payer,
            "accept Payment Request",
        )?;
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
            Some(expected_identity),
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
        let (_, expected_identity) = self.load_session_access_and_refresh_identity().await?;
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_role(
            &record,
            PaymentRequestLocalRole::Payer,
            "reject Payment Request",
        )?;
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
            Some(expected_identity),
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
        let (_, expected_identity) = self.load_session_access_and_refresh_identity().await?;
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
        if record.local_role == Some(PaymentRequestLocalRole::Payee) {
            require_payment_request_action_app(
                &record,
                &self.config.app_id,
                "cancel Payment Request",
            )?;
        }
        let event =
            PaymentRequestCancellation::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_raw_payment_request_response(
            counterparty.clone(),
            &PaymentRequestEvent::Cancellation(event),
            Some(expected_identity),
        )
        .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
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
        payment_request_id: &PaymentRequestId,
        billing_period: Option<BillingPeriod>,
        payment_app_id: paykit_lib::PaykitAppId,
        payment_endpoint_identifier: PaymentEndpointIdentifier,
        proof: JsonMap<String, JsonValue>,
    ) -> Result<PaymentRequestRecord> {
        self.submit_payment_proof_submission(
            counterparty,
            payment_request_id,
            PaymentProofSubmission {
                conversion_quote_id: None,
                billing_period,
                payment_app_id,
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
    /// and do not imply another payment or settlement confirmation. Only that
    /// occurrence's original proof sender may correct it, including after the
    /// execution claim moves to another App.
    /// Reporting evidence does not require the payee App to remain registered.
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
        payment_request_id: &PaymentRequestId,
        submission: PaymentProofSubmission,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_role(
            &record,
            PaymentRequestLocalRole::Payer,
            "submit Payment Proof",
        )?;
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
            submission.payment_app_id,
            submission.payment_endpoint_identifier,
            submission.proof,
        );
        if let Some(allowance_id) = submission.allowance_id {
            event = event.with_allowance_id(allowance_id);
        }
        if let Some(conversion_quote_id) = submission.conversion_quote_id {
            event = event.with_conversion_quote_id(conversion_quote_id);
        }
        validate_proof_conversion(&record, &event, &request)?;
        self.enqueue_raw_payment_proof(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Issue immutable rates for an accepted recurring request.
    /// Role and active request state are checked atomically with queueing; no payment is authorized.
    /// The caller establishes peer support and owns Pubky session creation, capability scope and key rotation.
    pub async fn quote_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        billing_period: BillingPeriod,
        rates: Vec<ConversionRate>,
        expires_at: String,
    ) -> Result<PaymentRequestRecord> {
        let _identity_guard = self.claim_identity_operation("quote Payment Request")?;
        let _session_access = self.ensure_private_outbound_ready(&counterparty).await?;
        self.enqueue_payment_conversion_quote(
            &counterparty,
            payment_request_id,
            billing_period,
            rates,
            expires_at,
        )
        .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    pub(super) async fn enqueue_payment_conversion_quote(
        &self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        billing_period: BillingPeriod,
        rates: Vec<ConversionRate>,
        expires_at: String,
    ) -> Result<()> {
        let clock = self.clock.clone();
        self.storage
            .transaction(|tx| {
                // Sample time only after the storage adapter has acquired its
                // transaction fence so a waiting quote cannot be backdated.
                let now = clock.now();
                let record =
                    crate::domain::payment_requests::payment_request_records_from_transaction(
                        tx,
                        counterparty,
                        now,
                    )?
                    .into_iter()
                    .find(|record| record.payment_request_id == payment_request_id.as_str())
                    .ok_or_else(|| PaykitSdkError::Policy {
                        context: "Payment Request was not found".into(),
                        source: None,
                    })?;
                require_role(
                    &record,
                    PaymentRequestLocalRole::Payee,
                    "quote Payment Request",
                )?;
                require_state(
                    &record,
                    &[PaymentRequestLifecycleState::ActiveRecurring],
                    "quote Payment Request",
                )?;
                require_payment_request_action_app(
                    &record,
                    &self.config.app_id,
                    "quote Payment Request",
                )?;
                let request =
                    request_from_record(&record).ok_or_else(|| PaykitSdkError::Protocol {
                        context: "Payment Request terms are unavailable".into(),
                        source: None,
                    })?;
                let expiry = DateTime::parse_from_rfc3339(&expires_at).map_err(|_| {
                    PaykitSdkError::Policy {
                        context: "invalid quote expiry".into(),
                        source: None,
                    }
                })?;
                if expiry < now {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot issue an expired conversion quote".into(),
                        source: None,
                    });
                }
                let quote = PaymentConversionQuote::new(
                    EventId::new_v4(),
                    payment_request_id.clone(),
                    billing_period,
                    rates,
                    now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    expires_at,
                )?;
                // Whole-second issuance matches the resolution of common payment timestamps.
                quote.validate_for_request(&request)?;
                let raw_json = paykit_lib::serialize_payment_request_event(
                    &self.config.app_id,
                    &PaymentRequestEvent::ConversionQuote(quote),
                )?;
                let (app_id, kind) =
                    crate::domain::outbound_private::validate_outbound_private_message(&raw_json)?;
                crate::storage::require_paykit_app_capability(
                    tx,
                    &app_id,
                    paykit_lib::PrivateMessageKind::PaymentConversionQuote,
                )?;
                tx.insert_outbound_private_message(
                    crate::storage::NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        self.config.app_id.clone(),
                        kind,
                        raw_json,
                        now,
                    ),
                )?;
                Ok(())
            })
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
        mark_payment_requests_recovery_required(records);
        Ok(())
    }

    pub(crate) async fn enqueue_raw_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequest,
    ) -> Result<PaymentRequestRecord> {
        self.with_storage_operation(Box::pin(async {
            self.ensure_private_outbound_ready(&counterparty).await?;
            enqueue_payment_request_message(
                &self.storage,
                counterparty,
                &self.config.app_id,
                event,
                self.clock.now(),
            )
            .await
        }))
        .await
    }

    async fn enqueue_raw_payment_request_response(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestEvent,
        expected_identity: Option<IdentityState>,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.with_storage_operation(Box::pin(async {
            let _session = self.ensure_private_outbound_ready(&counterparty).await?;
            crate::domain::payment_requests::enqueue_checked_payment_request_action_with_identity(
                &self.storage,
                counterparty,
                &self.config.app_id,
                event,
                || self.clock.now(),
                expected_identity,
            )
            .await
        }))
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
            None,
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

fn require_role(
    record: &PaymentRequestRecord,
    role: PaymentRequestLocalRole,
    action: &str,
) -> Result<()> {
    if record.local_role == Some(role) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: local identity is not the {}",
                match role {
                    PaymentRequestLocalRole::Payer => "payer",
                    PaymentRequestLocalRole::Payee => "payee",
                }
            ),
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

fn filtered_payment_request_records(
    tx: &dyn StorageTransaction,
    filter: &PaymentRequestFilter,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>> {
    let counterparties = if let Some(counterparty) = &filter.counterparty {
        if tx
            .linked_peer(counterparty)
            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
        {
            return Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            });
        }
        vec![counterparty.clone()]
    } else {
        let snapshot = tx.export_storage_state();
        let mut peers = HashSet::new();
        for item in snapshot.private_stream_items {
            if is_payment_request_kind(item.parsed_kind.as_deref()) {
                peers.insert(item.counterparty);
            }
        }
        if !filter.received_only {
            for outbound in snapshot.outbound_private_messages {
                if is_payment_request_kind(Some(&outbound.kind)) {
                    peers.insert(outbound.counterparty);
                }
            }
        }
        let mut peers = peers
            .into_iter()
            .filter(|peer| {
                !snapshot
                    .linked_peers
                    .get(peer)
                    .is_some_and(|record| record.state == LinkedPeerState::Blocked)
            })
            .collect::<Vec<_>>();
        peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        peers
    };
    let mut records = Vec::new();
    for peer in counterparties {
        let mut peer_records = if filter.received_only {
            received_payment_request_records_from_transaction(tx, &peer, now)?
        } else {
            payment_request_records_from_transaction(tx, &peer, now)?
        };
        if tx
            .linked_peer(&peer)
            .is_some_and(|record| record.state == LinkedPeerState::RecoveryRequired)
        {
            mark_payment_requests_recovery_required(&mut peer_records);
        }
        records.extend(
            peer_records
                .into_iter()
                .filter(|record| filter.matches(record)),
        );
    }
    sort_payment_requests_newest_first(&mut records);
    Ok(records)
}

fn mark_payment_requests_recovery_required(records: &mut [PaymentRequestRecord]) {
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
