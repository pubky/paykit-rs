use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Prepare a receipt issuance and persist it before network side effects.
    ///
    /// This does not store the Encrypted Receipt or queue Receipt Access. Use
    /// [`Self::process_receipt_issuance`] to continue the network steps, or
    /// [`Self::issue_receipt`] when the draft already has a Receipt ID.
    pub async fn prepare_receipt_issuance(
        &self,
        counterparty: PubkyPublicKey,
        draft: ReceiptDraft,
    ) -> Result<ReceiptIssuanceView> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "no local Pubky identity available for receipt issuance".into(),
                source: None,
            });
        }
        self.ensure_peer_not_blocked(&counterparty).await?;

        if let Some(receipt_id) = draft.receipt_id.as_ref() {
            if let Some(existing) =
                load_receipt_issuance_record_by_receipt_id(&self.storage, receipt_id.as_str())
                    .await?
            {
                if existing.app_id != self.config.app_id {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "Receipt issuance {} already exists for another Paykit App",
                            existing.receipt_id
                        ),
                        source: None,
                    });
                }
                if existing.counterparty != counterparty {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "Receipt issuance {} already exists for a different counterparty",
                            existing.receipt_id
                        ),
                        source: None,
                    });
                }
                if !receipt_issuance_record_matches_draft(&existing, &draft)? {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "Receipt issuance {} for counterparty {} already exists with different fields",
                            existing.receipt_id, counterparty
                        ),
                        source: None,
                    });
                }
                return Ok(ReceiptIssuanceView::from(&existing));
            }
        }

        let now = self.clock.now();
        let recipient = counterparty.to_public_key()?;
        let prepared = paykit_lib::prepare_receipt_for_recipient(recipient, draft)?;
        let record = ReceiptIssuanceRecord::from_prepared(
            counterparty,
            self.config.app_id.clone(),
            prepared,
            now,
        )?;
        self.storage
            .transaction({
                let app_id = self.config.app_id.clone();
                let record = record.clone();
                move |tx| {
                    crate::storage::require_paykit_app_capability(
                        tx,
                        &app_id,
                        PrivateMessageKind::ReceiptAccess,
                    )?;
                    if tx
                        .receipt_issuance_record_by_receipt_id(&record.receipt_id)
                        .is_some()
                    {
                        return Err(PaykitSdkError::Protocol {
                            context: format!(
                                "Receipt issuance {} already exists",
                                record.receipt_id
                            ),
                            source: None,
                        });
                    }
                    tx.save_receipt_issuance_record(record);
                    Ok(())
                }
            })
            .await?;
        Ok(ReceiptIssuanceView::from(&record))
    }

    /// Prepare, store, and queue Receipt Access for private delivery.
    ///
    /// The draft must include a Receipt ID so repeated calls are retry-safe.
    /// The returned record reflects local issuance progress. Receipt Access
    /// delivery still depends on processing the outbound private queue.
    pub async fn issue_receipt(
        &self,
        counterparty: PubkyPublicKey,
        draft: ReceiptDraft,
    ) -> Result<ReceiptIssuanceView> {
        if draft.receipt_id.is_none() {
            return Err(PaykitSdkError::Protocol {
                context: "issue_receipt requires a caller-provided Receipt ID for retry-safe issuance; use prepare_receipt_issuance first when the SDK should generate one".into(),
                source: None,
            });
        }
        let record = self
            .prepare_receipt_issuance(counterparty.clone(), draft)
            .await?;
        self.process_receipt_issuance(counterparty, &record.receipt_id)
            .await
    }

    /// Continue storage and Receipt Access queueing for a prepared issuance.
    pub async fn process_receipt_issuance(
        &self,
        counterparty: PubkyPublicKey,
        receipt_id: &str,
    ) -> Result<ReceiptIssuanceView> {
        let record = load_receipt_issuance_record(&self.storage, &counterparty, receipt_id)
            .await?
            .ok_or_else(|| PaykitSdkError::NotFound {
                context: format!(
                    "Receipt issuance {receipt_id} for counterparty {counterparty} was not found"
                ),
                source: None,
            })?;
        if record.app_id != self.config.app_id {
            return Err(PaykitSdkError::Policy {
                context: format!("Receipt issuance {receipt_id} belongs to another Paykit App"),
                source: None,
            });
        }
        if record.status == ReceiptIssuanceStatus::AccessQueued {
            return Ok(ReceiptIssuanceView::from(&record));
        }
        self.ensure_private_outbound_ready(&counterparty).await?;
        let (session_access, _) = self.private_link_session_access().await?;

        let record = if record.stored_at.is_some() {
            record
        } else {
            match store_encrypted_receipt_json(&session_access.session, &record).await {
                Ok(()) => {
                    let stored_at = self.clock.now();
                    let counterparty = counterparty.clone();
                    let receipt_id = receipt_id.to_owned();
                    self.storage
                        .transaction({
                            move |tx| {
                                let current = tx
                                    .receipt_issuance_record(&counterparty, &receipt_id)
                                    .ok_or_else(|| PaykitSdkError::NotFound {
                                        context: format!(
                                            "Receipt issuance {receipt_id} was not found"
                                        ),
                                        source: None,
                                    })?;
                                if current.status == ReceiptIssuanceStatus::AccessQueued
                                    || current.stored_at.is_some()
                                {
                                    return Ok(current);
                                }
                                let stored = current.mark_stored(stored_at);
                                tx.save_receipt_issuance_record(stored.clone());
                                Ok(stored)
                            }
                        })
                        .await?
                }
                Err(err) => {
                    self.save_receipt_issuance_failure(
                        &counterparty,
                        receipt_id,
                        self.clock.now(),
                        err.to_string(),
                    )
                    .await?;
                    return Err(err);
                }
            }
        };

        match enqueue_receipt_access_for_issuance(&self.storage, record.clone(), self.clock.now())
            .await
        {
            Ok(queued) => Ok(ReceiptIssuanceView::from(&queued)),
            Err(err) => {
                self.save_receipt_issuance_failure(
                    &counterparty,
                    receipt_id,
                    self.clock.now(),
                    err.to_string(),
                )
                .await?;
                Err(err)
            }
        }
    }

    /// List local receipt issuance records for one counterparty.
    pub async fn receipt_issuance_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<ReceiptIssuanceView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        let mut records = load_receipt_issuance_records(&self.storage, counterparty)
            .await?
            .iter()
            .map(ReceiptIssuanceView::from)
            .collect::<Vec<_>>();
        records.sort_by_key(|record| Reverse(record.created_at));
        Ok(records)
    }

    /// List issued receipts for one counterparty, newest first.
    pub async fn issued_receipts_to(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<ReceiptIssuanceView>> {
        self.receipt_issuance_records(counterparty).await
    }

    /// List issued receipts across non-blocked counterparties, newest first.
    pub async fn issued_receipts(&self) -> Result<Vec<ReceiptIssuanceView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.storage
            .transaction(|tx| {
                let snapshot = tx.export_storage_state();
                let mut records = snapshot
                    .receipt_issuance_records
                    .into_values()
                    .filter(|record| {
                        !snapshot
                            .linked_peers
                            .get(&record.counterparty)
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .map(|record| ReceiptIssuanceView::from(&record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.created_at));
                Ok(records)
            })
            .await
    }

    pub(in crate::runtime) async fn save_receipt_issuance_failure(
        &self,
        counterparty: &PubkyPublicKey,
        receipt_id: &str,
        failed_at: DateTime<Utc>,
        error: String,
    ) -> Result<()> {
        self.storage
            .transaction({
                let counterparty = counterparty.clone();
                let receipt_id = receipt_id.to_owned();
                move |tx| {
                    let current = tx
                        .receipt_issuance_record(&counterparty, &receipt_id)
                        .ok_or_else(|| PaykitSdkError::NotFound {
                            context: format!("Receipt issuance {receipt_id} was not found"),
                            source: None,
                        })?;
                    if current.status == ReceiptIssuanceStatus::AccessQueued
                        || current.stored_at.is_some()
                    {
                        return Ok(());
                    }
                    tx.save_receipt_issuance_record(current.mark_failed(failed_at, error));
                    Ok(())
                }
            })
            .await
    }
}
