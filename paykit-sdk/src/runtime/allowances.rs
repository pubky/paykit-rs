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
        let scopes = self
            .storage
            .transaction(|tx| {
                let snapshot = tx.export_storage_state();
                let mut scopes = Vec::new();
                for (counterparty, receiver_path) in allowance_scopes(&snapshot) {
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
                    let blocked = snapshot
                        .linked_peers
                        .get(&(counterparty.clone(), receiver_path.clone()))
                        .is_some_and(|peer| peer.state == LinkedPeerState::Blocked);
                    if blocked && filter.counterparty.is_some() {
                        return Err(PaykitSdkError::Policy {
                            context: format!("counterparty {counterparty} is blocked"),
                            source: None,
                        });
                    }
                    if !blocked {
                        scopes.push((counterparty, receiver_path));
                    }
                }
                Ok(scopes)
            })
            .await?;
        let mut records = Vec::new();
        for (counterparty, receiver_path) in scopes {
            records.extend(
                derive_allowance_records(&self.storage, &counterparty, &receiver_path)
                    .await?
                    .into_iter()
                    .filter(|record| filter.matches(record)),
            );
        }
        sort_allowances_newest_first(&mut records);
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
        self.ensure_peer_not_blocked(counterparty, counterparty_receiver_path)
            .await?;
        derive_allowance_record(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id,
        )
        .await
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
        self.require_identity_and_session().await?;
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
    /// The lifecycle, role, history, link, and append checks occur in one
    /// durable transaction. The caller remains responsible for Pubky session
    /// creation, capability scope, key rotation, and timeouts.
    pub async fn accept_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<AllowanceRecord> {
        self.require_identity_and_session().await?;
        enqueue_allowance_response(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id.clone(),
            AllowanceResponse::Acceptance,
            self.clock.now(),
        )
        .await
    }

    /// Queue rejection as the authenticated proposal recipient.
    ///
    /// The lifecycle, role, history, link, and append checks occur in one
    /// durable transaction. The caller remains responsible for Pubky session
    /// creation, capability scope, key rotation, and timeouts.
    pub async fn reject_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<AllowanceRecord> {
        self.require_identity_and_session().await?;
        enqueue_allowance_response(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id.clone(),
            AllowanceResponse::Rejection,
            self.clock.now(),
        )
        .await
    }

    /// Queue a proposal withdrawal or unilateral End for accepted authority.
    ///
    /// End is the fail-safe terminal action: it is permitted on invalid or
    /// unresolved Allowance history and is blocked only while the exact
    /// Encrypted Link requires recovery. The lifecycle, role, link, and append
    /// checks occur in one durable transaction. The caller remains responsible
    /// for Pubky session creation, capability scope, key rotation, and
    /// timeouts.
    pub async fn end_allowance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: &AllowanceId,
    ) -> Result<AllowanceRecord> {
        self.require_identity_and_session().await?;
        enqueue_allowance_end(
            &self.storage,
            counterparty,
            counterparty_receiver_path,
            allowance_id.clone(),
            self.clock.now(),
        )
        .await
    }
}
