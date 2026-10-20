use super::*;

/// Live identity status and Payment Requests read in one storage operation.
///
/// This read does not authorize payment or make a later proof submission
/// idempotent. Callers must retain their exact request and proof checks.
#[derive(Clone, PartialEq)]
pub struct PaymentRequestRead {
    /// Current identity and session capability, when initialized.
    pub identity_status: Option<IdentityStatus>,
    /// Complete records for the requested counterparty, newest first.
    ///
    /// Empty when identity is uninitialized, is not private-link-capable, or
    /// does not match the expected payer.
    pub payment_requests: Vec<PaymentRequestRecord>,
}

impl std::fmt::Debug for PaymentRequestRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentRequestRead")
            .field(
                "capability",
                &self
                    .identity_status
                    .as_ref()
                    .map(|status| status.capability),
            )
            .field("payment_requests", &self.payment_requests.len())
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
    /// Read live identity status and one counterparty's complete Payment Requests.
    ///
    /// The identity and optional expected payer are checked before request
    /// listing refreshes durable identity/key bindings. Missing initialization
    /// is not repaired implicitly. Without a live session, only locally
    /// available identity metadata is read and no requests are returned.
    /// Existing storage/session access errors still propagate, including missing
    /// keys or scopes required to load Pubky shared state.
    ///
    /// Pubky shared storage loads fresh state once under its normal lock and
    /// recovery safeguards. Existing request-listing commits remain durable;
    /// an error does not roll back earlier transactions. Do not cache this
    /// result for payment checks or reuse it across a mutation. A later action
    /// still performs its own authorization and exact proof checks.
    ///
    /// Pubky shared storage reports `SharedStateBusy` only when acquiring the
    /// outer lock, before either read or any refresh commit. That acquisition
    /// failure can be retried; other failures must not trigger blanket replay.
    /// Session creation, capability scope and key rotation remain caller-owned.
    pub async fn read_payment_requests(
        &self,
        counterparty: &PubkyPublicKey,
        expected_identity: Option<&PubkyPublicKey>,
    ) -> Result<PaymentRequestRead> {
        let guard = self.session_read_guard().await?;
        if self.pubky.load_session_access().await?.is_none() {
            return Ok(PaymentRequestRead {
                identity_status: self
                    .load_signed_out_identity_state()
                    .await?
                    .map(|state| IdentityStatus::from_state(&state, false, false)),
                payment_requests: Vec::new(),
            });
        }
        self.with_guarded_storage_operation(
            guard,
            Box::pin(async {
                let identity_status = self.identity_status().await?;
                let can_read = identity_status.as_ref().is_some_and(|status| {
                    status.capability == PubkyIdentityCapability::PrivateLinkCapable
                        && expected_identity
                            .is_none_or(|expected| status.public_key.as_ref() == Some(expected))
                });
                let payment_requests = if can_read {
                    self.payment_requests_with(counterparty).await?
                } else {
                    Vec::new()
                };
                Ok(PaymentRequestRead {
                    identity_status,
                    payment_requests,
                })
            }),
        )
        .await
    }
}
