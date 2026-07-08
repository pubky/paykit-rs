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
        counterparty_receiver_path: PaykitReceiverPath,
        draft: ReceiptDraft,
    ) -> Result<ReceiptIssuanceView> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "no local Pubky identity available for receipt issuance".into(),
                source: None,
            });
        }
        self.ensure_peer_not_blocked(&counterparty, &counterparty_receiver_path)
            .await?;

        if let Some(receipt_id) = draft.receipt_id.as_ref() {
            if let Some(existing) =
                load_receipt_issuance_record_by_receipt_id(&self.storage, receipt_id.as_str())
                    .await?
            {
                if existing.counterparty != counterparty
                    || existing.counterparty_receiver_path != counterparty_receiver_path
                {
                    return Err(PaykitSdkError::Protocol(format!(
                        "Receipt issuance {} already exists for a different counterparty receiver",
                        existing.receipt_id
                    )));
                }
                if !receipt_issuance_record_matches_draft(
                    &existing,
                    &counterparty_receiver_path,
                    &draft,
                )? {
                    return Err(PaykitSdkError::Protocol(format!(
                        "Receipt issuance {} for counterparty {} already exists with different fields",
                        existing.receipt_id, counterparty
                    )));
                }
                return Ok(ReceiptIssuanceView::from(&existing));
            }
        }

        let now = self.clock.now();
        let recipient = counterparty.to_public_key()?;
        let prepared = paykit_lib::prepare_receipt_for_recipient(
            recipient,
            &self.config.receiver_path,
            draft,
        )?;
        let record = ReceiptIssuanceRecord::from_prepared(
            counterparty,
            counterparty_receiver_path,
            prepared,
            now,
        )?;
        self.storage
            .transaction({
                let record = record.clone();
                move |tx| {
                    if tx
                        .receipt_issuance_record_by_receipt_id(&record.receipt_id)
                        .is_some()
                    {
                        return Err(PaykitSdkError::Protocol(format!(
                            "Receipt issuance {} already exists",
                            record.receipt_id
                        )));
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
        counterparty_receiver_path: PaykitReceiverPath,
        draft: ReceiptDraft,
    ) -> Result<ReceiptIssuanceView> {
        if draft.receipt_id.is_none() {
            return Err(PaykitSdkError::Protocol(
                "issue_receipt requires a caller-provided Receipt ID for retry-safe issuance; use prepare_receipt_issuance first when the SDK should generate one".into(),
            ));
        }
        let record = self
            .prepare_receipt_issuance(
                counterparty.clone(),
                counterparty_receiver_path.clone(),
                draft,
            )
            .await?;
        self.process_receipt_issuance(counterparty, counterparty_receiver_path, &record.receipt_id)
            .await
    }

    /// Continue storage and Receipt Access queueing for a prepared issuance.
    pub async fn process_receipt_issuance(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        receipt_id: &str,
    ) -> Result<ReceiptIssuanceView> {
        let (session_access, _) = self.private_link_session_access().await?;
        let record = load_receipt_issuance_record(
            &self.storage,
            &counterparty,
            &counterparty_receiver_path,
            receipt_id,
        )
        .await?
        .ok_or_else(|| {
                PaykitSdkError::NotFound(format!(
                    "Receipt issuance {receipt_id} for counterparty {counterparty}/{counterparty_receiver_path} was not found"
                ))
            })?;
        self.ensure_private_outbound_ready(&counterparty, &record.counterparty_receiver_path)
            .await?;
        if record.status == ReceiptIssuanceStatus::AccessQueued {
            return Ok(ReceiptIssuanceView::from(&record));
        }

        let record = if record.stored_at.is_some() {
            record
        } else {
            match store_encrypted_receipt_json(&session_access.session, &record).await {
                Ok(()) => {
                    let stored = record.mark_stored(self.clock.now());
                    self.storage
                        .transaction({
                            let stored = stored.clone();
                            move |tx| {
                                tx.save_receipt_issuance_record(stored);
                                Ok(())
                            }
                        })
                        .await?;
                    stored
                }
                Err(err) => {
                    let failed = record.mark_failed(self.clock.now(), err.to_string());
                    self.storage
                        .transaction({
                            let failed = failed.clone();
                            move |tx| {
                                tx.save_receipt_issuance_record(failed);
                                Ok(())
                            }
                        })
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
                let failed = record.mark_failed(self.clock.now(), err.to_string());
                self.storage
                    .transaction({
                        let failed = failed.clone();
                        move |tx| {
                            tx.save_receipt_issuance_record(failed);
                            Ok(())
                        }
                    })
                    .await?;
                Err(err)
            }
        }
    }

    /// List local receipt issuance records for one counterparty.
    pub async fn receipt_issuance_records(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<ReceiptIssuanceView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty, counterparty_receiver_path)
            .await?;
        let mut records =
            load_receipt_issuance_records(&self.storage, counterparty, counterparty_receiver_path)
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
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<ReceiptIssuanceView>> {
        self.receipt_issuance_records(counterparty, counterparty_receiver_path)
            .await
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
                            .get(&(
                                record.counterparty.clone(),
                                record.counterparty_receiver_path.clone(),
                            ))
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .map(|record| ReceiptIssuanceView::from(&record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.created_at));
                Ok(records)
            })
            .await
    }

    /// Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
    ///
    /// The decrypted Receipt is private SDK state. This returns an already
    /// stored Receipt record when available, and otherwise validates the
    /// decrypted recipient against the current local Pubky identity before
    /// saving it.
    pub async fn retrieve_receipt(
        &self,
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        receipt_id: &str,
    ) -> Result<ReceiptRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local_public_key = identity
            .public_key
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "no local Pubky identity available for receipt retrieval".into(),
                source: None,
            })?;
        self.ensure_peer_not_blocked(&counterparty, &counterparty_receiver_path)
            .await?;
        let (stored_receipt, access_records, conflicted_access_count, stored_receipt_conflicted) =
            self.storage
                .transaction(|tx| {
                    let stored_receipt =
                        tx.receipt_record(&counterparty, &counterparty_receiver_path, receipt_id);
                    let stored_receipt_conflicted = stored_receipt.as_ref().is_some_and(|record| {
                        tx.event_dedup_record(
                            &counterparty,
                            &counterparty_receiver_path,
                            &record.receipt_access_event_id,
                        )
                        .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
                    });
                    let mut access_records = Vec::new();
                    let mut conflicted_access_count = 0usize;
                    for record in tx
                        .receipt_access_records(&counterparty, &counterparty_receiver_path)
                        .into_iter()
                        .filter(|record| record.receipt_id == receipt_id)
                    {
                        if Self::receipt_access_event_is_conflicted(tx, &record) {
                            conflicted_access_count += 1;
                        } else {
                            access_records.push(record);
                        }
                    }
                    access_records.sort_by_key(|record| Reverse(record.stream_item_id));
                    Ok((
                        stored_receipt,
                        access_records,
                        conflicted_access_count,
                        stored_receipt_conflicted,
                    ))
                })
                .await?;
        if let Some(record) = stored_receipt {
            if record.recipient_public_key != local_public_key {
                return Err(PaykitSdkError::Protocol(
                    "stored Receipt recipient does not match local identity".into(),
                ));
            }
            if stored_receipt_conflicted {
                return Err(Self::conflicted_receipt_access_error(receipt_id));
            }
            if access_records.is_empty() && conflicted_access_count > 0 {
                return Err(Self::conflicted_receipt_access_error(receipt_id));
            }
            self.reconcile_cached_receipt_access_records(
                &record,
                &access_records,
                self.clock.now(),
            )
            .await?;
            return Ok(record);
        }
        if access_records.is_empty() && conflicted_access_count > 0 {
            return Err(Self::conflicted_receipt_access_error(receipt_id));
        }
        if access_records.is_empty() {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no Receipt Access record for receipt {receipt_id} from {counterparty}"
            )));
        }
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for receipt retrieval".into(),
                    source: None,
                })?;
        let now = self.clock.now();
        let all_access_records = access_records.clone();
        let mut last_error = None;
        for access in access_records {
            let encrypted_json = match fetch_encrypted_receipt_json(
                &public_storage,
                &counterparty,
                &access.location,
            )
            .await
            {
                Ok(Some(encrypted_json)) => encrypted_json,
                Ok(None) => {
                    let error = format!(
                        "encrypted receipt {} was not found at {}",
                        access.receipt_id, access.location
                    );
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::NotFound,
                        now,
                        error.clone(),
                    )
                    .await?;
                    last_error = Some(PaykitSdkError::Transport {
                        context: error,
                        source: None,
                    });
                    continue;
                }
                Err(err) => {
                    let error = err.to_string();
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::Failed,
                        now,
                        error,
                    )
                    .await?;
                    last_error = Some(err);
                    continue;
                }
            };

            match decrypt_receipt_record_from_access(
                &access,
                &encrypted_json,
                now,
                &local_public_key,
            ) {
                Ok(record) => {
                    self.storage
                        .transaction({
                            let access = access.mark_retrieved(now);
                            let record = record.clone();
                            move |tx| {
                                tx.save_receipt_access_record(access);
                                tx.save_receipt_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    self.reconcile_cached_receipt_access_records(&record, &all_access_records, now)
                        .await?;
                    return Ok(record);
                }
                Err(err) => {
                    let error = err.to_string();
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::Failed,
                        now,
                        error,
                    )
                    .await?;
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            PaykitSdkError::RecoveryRequired(format!(
                "no usable Receipt Access record for receipt {receipt_id} from {counterparty}"
            ))
        }))
    }

    /// List indexed Receipt Access records for one counterparty.
    pub async fn receipt_access_records(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<ReceiptAccessView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty, counterparty_receiver_path)
            .await?;
        self.storage
            .transaction(|tx| {
                let mut records = tx
                    .receipt_access_records(counterparty, counterparty_receiver_path)
                    .into_iter()
                    .filter(|record| !Self::receipt_access_event_is_conflicted(tx, record))
                    .map(|record| ReceiptAccessView::from(&record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.received_at));
                Ok(records)
            })
            .await
    }

    /// List Receipt Access received from one counterparty.
    pub async fn receipt_access_from(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<ReceiptAccessView>> {
        self.receipt_access_records(counterparty, counterparty_receiver_path)
            .await
    }

    /// List Receipt Access across non-blocked counterparties, newest first.
    pub async fn receipt_access(&self) -> Result<Vec<ReceiptAccessView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.storage
            .transaction(|tx| {
                let snapshot = tx.export_storage_state();
                let mut records = snapshot
                    .receipt_access_records
                    .into_values()
                    .filter(|record| {
                        !snapshot
                            .linked_peers
                            .get(&(
                                record.counterparty.clone(),
                                record.counterparty_receiver_path.clone(),
                            ))
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .filter(|record| !Self::receipt_access_event_is_conflicted(tx, record))
                    .map(|record| ReceiptAccessView::from(&record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.received_at));
                Ok(records)
            })
            .await
    }

    /// List decrypted Receipt records for one issuer, newest first.
    pub async fn receipt_records(
        &self,
        issuer: &PubkyPublicKey,
        issuer_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<ReceiptRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let Some(local_public_key) = identity.public_key else {
            return Ok(Vec::new());
        };
        self.ensure_peer_not_blocked(issuer, issuer_receiver_path)
            .await?;
        self.storage
            .transaction(|tx| {
                let mut records = tx
                    .export_storage_state()
                    .receipt_records
                    .into_values()
                    .filter(|record| &record.issuer == issuer)
                    .filter(|record| &record.issuer_receiver_path == issuer_receiver_path)
                    .filter(|record| record.recipient_public_key == local_public_key)
                    .filter(|record| !Self::receipt_record_access_event_is_conflicted(tx, record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.retrieved_at));
                Ok(records)
            })
            .await
    }

    /// List decrypted receipts from one issuer, newest first.
    pub async fn receipts_from(
        &self,
        issuer: &PubkyPublicKey,
        issuer_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<ReceiptRecord>> {
        self.receipt_records(issuer, issuer_receiver_path).await
    }

    /// List decrypted receipts across non-blocked issuers, newest first.
    pub async fn receipts(&self) -> Result<Vec<ReceiptRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let Some(local_public_key) = identity.public_key else {
            return Ok(Vec::new());
        };
        self.storage
            .transaction(|tx| {
                let snapshot = tx.export_storage_state();
                let mut records = snapshot
                    .receipt_records
                    .into_values()
                    .filter(|record| record.recipient_public_key == local_public_key)
                    .filter(|record| {
                        !snapshot
                            .linked_peers
                            .get(&(record.issuer.clone(), record.issuer_receiver_path.clone()))
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .filter(|record| !Self::receipt_record_access_event_is_conflicted(tx, record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.retrieved_at));
                Ok(records)
            })
            .await
    }

    async fn reconcile_cached_receipt_access_records(
        &self,
        record: &ReceiptRecord,
        access_records: &[ReceiptAccessRecord],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let has_mismatched_access = self
            .storage
            .transaction({
                let record = record.clone();
                let access_records = access_records.to_vec();
                move |tx| {
                    let mut has_mismatched_access = false;
                    for access in access_records {
                        if receipt_record_matches_access(&record, &access) {
                            if access.retrieval_status != ReceiptRetrievalStatus::Retrieved {
                                tx.save_receipt_access_record(access.mark_retrieved(now));
                            }
                        } else {
                            has_mismatched_access = true;
                            if access.retrieval_status == ReceiptRetrievalStatus::Pending {
                                tx.save_receipt_access_record(access.mark_retrieval_error(
                                    ReceiptRetrievalStatus::Failed,
                                    now,
                                    "Receipt Access does not match stored Receipt".into(),
                                ));
                            }
                        }
                    }
                    Ok(has_mismatched_access)
                }
            })
            .await?;
        if has_mismatched_access {
            return Err(Self::mismatched_receipt_access_error(&record.receipt_id));
        }
        Ok(())
    }

    fn receipt_access_event_is_conflicted(
        tx: &dyn crate::storage::StorageTransaction,
        access: &ReceiptAccessRecord,
    ) -> bool {
        tx.event_dedup_record(
            &access.counterparty,
            &access.counterparty_receiver_path,
            &access.event_id,
        )
        .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
    }

    fn receipt_record_access_event_is_conflicted(
        tx: &dyn crate::storage::StorageTransaction,
        record: &ReceiptRecord,
    ) -> bool {
        tx.event_dedup_record(
            &record.issuer,
            &record.issuer_receiver_path,
            &record.receipt_access_event_id,
        )
        .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
    }

    fn conflicted_receipt_access_error(receipt_id: &str) -> PaykitSdkError {
        PaykitSdkError::Protocol(format!(
            "Receipt Access event for receipt {receipt_id} has a conflicting Event ID"
        ))
    }

    fn mismatched_receipt_access_error(receipt_id: &str) -> PaykitSdkError {
        PaykitSdkError::Protocol(format!(
            "Receipt Access descriptor for receipt {receipt_id} conflicts with stored Receipt"
        ))
    }

    async fn save_receipt_retrieval_error(
        &self,
        access: &ReceiptAccessRecord,
        status: ReceiptRetrievalStatus,
        attempted_at: DateTime<Utc>,
        error: String,
    ) -> Result<()> {
        self.storage
            .transaction({
                let access = access.mark_retrieval_error(status, attempted_at, error);
                move |tx| {
                    tx.save_receipt_access_record(access);
                    Ok(())
                }
            })
            .await
    }
}
