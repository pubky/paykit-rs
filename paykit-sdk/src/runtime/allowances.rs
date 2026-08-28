use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Return Allowances matching a local SDK filter.
    ///
    /// Results retain the exact counterparty and receiver path and are sorted
    /// newest-first by local record time. Lifecycle state is not an
    /// eligibility or payment-authorization decision.
    pub async fn list_allowances(&self, filter: AllowanceFilter) -> Result<Vec<AllowanceRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.local_pubky_public_key.is_none() {
            return Ok(Vec::new());
        }
        let state = self
            .storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await?;
        let mut records = Vec::new();
        for (counterparty, receiver_path) in allowance_scopes(&state) {
            if filter
                .counterparty
                .as_ref()
                .is_some_and(|expected| expected != &counterparty)
                || filter
                    .counterparty_receiver_path
                    .as_ref()
                    .is_some_and(|expected| expected != &receiver_path)
            {
                continue;
            }
            let blocked = state
                .linked_peers
                .get(&(counterparty.clone(), receiver_path.clone()))
                .is_some_and(|peer| peer.state == LinkedPeerState::Blocked);
            if blocked && filter.counterparty.is_some() {
                return Err(PaykitSdkError::Policy {
                    context: format!("counterparty {counterparty} is blocked"),
                    source: None,
                });
            }
            if blocked {
                continue;
            }
            records.extend(
                allowance_records_from_state(&state, &counterparty, &receiver_path)
                    .into_iter()
                    .filter(|record| filter.matches(record)),
            );
        }
        sort_allowance_records_newest_first(&mut records);
        Ok(records)
    }

    /// Return one Allowance from one exact authenticated Encrypted Link.
    pub async fn allowance_record(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<Option<AllowanceRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.local_pubky_public_key.is_none() {
            return Ok(None);
        }
        let state = self
            .storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await?;
        if state
            .linked_peers
            .get(&(counterparty.clone(), counterparty_receiver_path.clone()))
            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
        {
            return Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            });
        }
        Ok(allowance_record_from_state(
            &state,
            counterparty,
            counterparty_receiver_path,
            allowance_id,
        ))
    }

    /// Queue a proposal with fresh Allowance and Event IDs.
    ///
    /// The returned record reflects local durable outbound intent, not remote
    /// delivery. The caller remains responsible for Pubky session creation,
    /// capability scope, key rotation, and request-timeout configuration.
    pub async fn propose_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        local_role: AllowanceLocalRole,
        terms: AllowanceTerms,
    ) -> Result<AllowanceRecord> {
        self.require_allowance_outbound_identity().await?;
        enqueue_allowance_proposal(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            local_role,
            terms,
            self.clock.now(),
        )
        .await
    }

    /// Queue acceptance as the authenticated proposal recipient.
    ///
    /// The lifecycle, role, history, link, Event ID, and append checks occur in
    /// one durable transaction. The caller remains responsible for Pubky
    /// session creation, capability scope, key rotation, and timeouts.
    pub async fn accept_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<AllowanceRecord> {
        self.require_allowance_outbound_identity().await?;
        enqueue_allowance_acceptance(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id.clone(),
            self.clock.now(),
        )
        .await
    }

    /// Queue rejection as the authenticated proposal recipient.
    ///
    /// The lifecycle, role, history, link, Event ID, and append checks occur in
    /// one durable transaction. The caller remains responsible for Pubky
    /// session creation, capability scope, key rotation, and timeouts.
    pub async fn reject_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<AllowanceRecord> {
        self.require_allowance_outbound_identity().await?;
        enqueue_allowance_rejection(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id.clone(),
            self.clock.now(),
        )
        .await
    }

    /// Queue a proposal withdrawal or unilateral End for accepted authority.
    ///
    /// The lifecycle, role, history, link, Event ID, and append checks occur in
    /// one durable transaction. The caller remains responsible for Pubky
    /// session creation, capability scope, key rotation, and timeouts.
    pub async fn end_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<AllowanceRecord> {
        self.require_allowance_outbound_identity().await?;
        enqueue_allowance_end(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id.clone(),
            self.clock.now(),
        )
        .await
    }

    async fn require_allowance_outbound_identity(&self) -> Result<()> {
        let (session, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.local_pubky_public_key.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "local Pubky identity is not initialized".into(),
                source: None,
            });
        }
        if session.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "no Pubky session available".into(),
                source: None,
            });
        }
        Ok(())
    }
}

fn sort_allowance_records_newest_first(records: &mut [AllowanceRecord]) {
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
            .then_with(|| left.allowance_id.cmp(&right.allowance_id))
    });
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use chrono::TimeZone;
    use paykit_lib::{
        serialize_allowance_event, AllowanceEvent, AllowanceProposal, AllowanceRole, EventId,
        PrivateApplicationMessage,
    };

    use super::*;
    use crate::{
        domain::private_stream::persist_private_stream_batch, storage::InMemoryStorage,
        IdentityState,
    };

    #[derive(Clone)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap()
        }
    }

    struct NoSession;

    #[async_trait]
    impl PubkySessionProvider for NoSession {
        async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
            Ok(None)
        }

        async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
            Ok(None)
        }

        async fn clear_session_access(&self) -> Result<()> {
            Ok(())
        }
    }

    struct NoPayment;

    #[async_trait]
    impl PaymentAdapter for NoPayment {}

    fn peer() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    fn path(value: &str) -> PaykitReceiverPath {
        PaykitReceiverPath::new(value).unwrap()
    }

    fn allowance_message(
        allowance_id: &str,
        event_id: &str,
        proposer_role: AllowanceRole,
    ) -> PrivateApplicationMessage {
        let event = AllowanceEvent::Proposal(AllowanceProposal::new(
            EventId::new(event_id).unwrap(),
            AllowanceId::new(allowance_id).unwrap(),
            proposer_role,
            AllowanceTerms::builder("btc")
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        ));
        PrivateApplicationMessage {
            version: Some(1),
            kind: Some(event.kind().as_str().to_owned()),
            raw_json: serialize_allowance_event(&event).unwrap(),
        }
    }

    #[tokio::test]
    async fn test_list_and_get_allowances_preserve_exact_link_scope() {
        let storage = InMemoryStorage::new();
        let counterparty = peer();
        storage
            .save_identity_state(IdentityState {
                local_pubky_public_key: Some(peer()),
                local_receiver_noise_public_key: Some(peer()),
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            })
            .await
            .unwrap();
        let wallet_allowance_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";
        let server_allowance_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab45";
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            path("bitkit/wallet"),
            vec![allowance_message(
                wallet_allowance_id,
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
                AllowanceRole::Allower,
            )],
            None,
            FixedClock.now(),
        )
        .await
        .unwrap();
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            path("bitkit/server"),
            vec![allowance_message(
                server_allowance_id,
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
                AllowanceRole::Allowee,
            )],
            None,
            FixedClock.now(),
        )
        .await
        .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage,
            NoSession,
            NoPayment,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let listed = sdk
            .list_allowances(AllowanceFilter {
                counterparty: Some(counterparty.clone()),
                counterparty_receiver_path: Some(path("bitkit/wallet")),
                local_role: Some(AllowanceLocalRole::Allowee),
                ..AllowanceFilter::default()
            })
            .await
            .unwrap();
        let wrong_link = sdk
            .allowance_record(
                &counterparty,
                &path("bitkit/server"),
                &AllowanceId::new(wallet_allowance_id).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].allowance_id, wallet_allowance_id);
        assert_eq!(listed[0].counterparty_receiver_path, path("bitkit/wallet"));
        assert!(wrong_link.is_none());
    }
}
