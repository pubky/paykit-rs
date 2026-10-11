use super::*;

/// A coherent, observation-only view of previously stored Paykit data.
///
/// This is not payment authority or evidence that a peer is currently reachable.
/// Callers may cache it for display, scoped to its identity and key generation,
/// and must discard it when that scope changes or session access is cleared.
/// Payment admission and mutations must continue through the authoritative SDK
/// APIs, even when this observation appears current.
#[derive(Clone, PartialEq)]
pub struct PaykitStateObservation {
    /// Identity whose stored data was observed.
    pub public_key: PubkyPublicKey,
    /// Paykit key generation used to access the data.
    pub key_generation: u64,
    /// Local observation time, not a remote revision or freshness guarantee.
    pub observed_at: DateTime<Utc>,
    /// Stored contacts, sorted by public key.
    pub contacts: Vec<ContactRecord>,
    /// Stored link states, sorted by counterparty.
    pub linked_peers: Vec<LinkedPeerRecord>,
    /// Payment Requests from non-blocked counterparties, newest first.
    pub payment_requests: Vec<PaymentRequestRecord>,
    /// Previously decrypted Receipts with valid stored access, newest first.
    pub receipts: Vec<ReceiptRecord>,
}

impl std::fmt::Debug for PaykitStateObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaykitStateObservation")
            .field("public_key", &"<redacted>")
            .field("key_generation", &self.key_generation)
            .field("observed_at", &self.observed_at)
            .field("contacts", &self.contacts.len())
            .field("linked_peers", &self.linked_peers.len())
            .field("payment_requests", &self.payment_requests.len())
            .field("receipts", &self.receipts.len())
            .finish()
    }
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Read display observations together in one storage transaction.
    ///
    /// Requires an initialized identity and live, private-link-capable session.
    /// This does not receive peer messages, retrieve Receipts or refresh remote
    /// app authorization. Pubky shared storage retains its normal locking and
    /// uncertain-write safeguards; run this in a coalesced refresh worker, not
    /// on the display path. Keep the returned view in the app's identity-scoped
    /// cache and do not substitute it for payment eligibility or execution APIs.
    /// Session creation, capability scope and key rotation remain caller-owned.
    pub async fn observe_state(&self) -> Result<PaykitStateObservation> {
        let guard = self.session_read_guard().await?;
        let access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "cannot observe Paykit state without an active session".into(),
                    source: None,
                })?;
        access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        let secret =
            access
                .paykit_identity_secret_key()
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "cannot observe private Paykit state without its identity secret"
                        .into(),
                    source: None,
                })?;
        let public_key = access.public_key()?;
        let noise_public_key = crate::storage::paykit_noise_public_key(&secret);
        let key_generation = secret.key_generation();
        self.with_guarded_storage_operation(
            guard,
            Box::pin(async move {
                self.storage
                    .transaction(move |tx| {
                        Self::state_observation_in_transaction(
                            tx,
                            &public_key,
                            &noise_public_key,
                            key_generation,
                            self.clock.now(),
                        )
                    })
                    .await
            }),
        )
        .await
    }

    pub(super) fn state_observation_in_transaction(
        tx: &dyn StorageTransaction,
        public_key: &PubkyPublicKey,
        noise_public_key: &PubkyPublicKey,
        key_generation: u64,
        observed_at: DateTime<Utc>,
    ) -> Result<PaykitStateObservation> {
        let identity = initialized_identity_in_transaction(tx, "observe Paykit state")?;
        let snapshot = tx.export_storage_state();
        if &identity != public_key
            || snapshot.paykit_noise_public_key.as_ref() != Some(noise_public_key)
        {
            return Err(PaykitSdkError::Identity {
                context: "Paykit observation session does not match the stored identity and key"
                    .into(),
                source: None,
            });
        }
        let mut linked_peers = snapshot.linked_peers.into_values().collect::<Vec<_>>();
        linked_peers
            .sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));
        Ok(PaykitStateObservation {
            public_key: identity,
            key_generation,
            observed_at,
            contacts: tx.contact_records(),
            linked_peers,
            payment_requests: payment_requests::filtered_payment_request_records(
                tx,
                &PaymentRequestFilter::default(),
                observed_at,
            )?,
            receipts: Self::receipt_observations_in_transaction(tx, public_key),
        })
    }
}
