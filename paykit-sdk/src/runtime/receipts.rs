use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
    ///
    /// The decrypted Receipt is private SDK state. This returns an already
    /// stored Receipt record when available, and otherwise validates the
    /// decrypted recipient against the current local Pubky identity before
    /// saving it.
    pub async fn retrieve_receipt(
        &self,
        counterparty: PubkyPublicKey,
        receipt_id: &str,
    ) -> Result<ReceiptRecord> {
        self.ensure_private_workflows_enabled("Receipt retrieval")?;
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local_public_key = identity
            .public_key
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "no local Pubky identity available for receipt retrieval".into(),
                source: None,
            })?;
        self.ensure_peer_not_blocked(&counterparty).await?;
        let (stored_receipt, access_records, conflicted_access_count, stored_receipt_conflicted) =
            self.storage
                .transaction(|tx| {
                    let stored_receipt = tx.receipt_record(&counterparty, receipt_id);
                    let stored_receipt_conflicted = stored_receipt.as_ref().is_some_and(|record| {
                        tx.event_dedup_record(&counterparty, &record.receipt_access_event_id)
                            .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
                    });
                    let mut access_records = Vec::new();
                    let mut conflicted_access_count = 0usize;
                    for record in tx
                        .receipt_access_records(&counterparty)
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
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for receipt retrieval".into(),
                    source: None,
                })?;
        if access_records.is_empty() {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no Receipt Access record for receipt {receipt_id} from {counterparty}"
            )));
        }
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
    ) -> Result<Vec<ReceiptAccessView>> {
        self.ensure_private_workflows_enabled("Receipt Access record access")?;
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        self.storage
            .transaction(|tx| {
                let records = tx
                    .receipt_access_records(counterparty)
                    .into_iter()
                    .filter(|record| !Self::receipt_access_event_is_conflicted(tx, record))
                    .map(|record| ReceiptAccessView::from(&record))
                    .collect::<Vec<_>>();
                Ok(records)
            })
            .await
    }

    /// List decrypted Receipt records for one issuer, newest first.
    pub async fn receipt_records(&self, issuer: &PubkyPublicKey) -> Result<Vec<ReceiptRecord>> {
        self.ensure_private_workflows_enabled("Receipt record access")?;
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let Some(local_public_key) = identity.public_key else {
            return Ok(Vec::new());
        };
        self.ensure_peer_not_blocked(issuer).await?;
        self.storage
            .transaction(|tx| {
                let mut records = tx
                    .export_storage_state()
                    .receipt_records
                    .into_values()
                    .filter(|record| &record.issuer == issuer)
                    .filter(|record| record.recipient_public_key == local_public_key)
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
        self.storage
            .transaction({
                let record = record.clone();
                let access_records = access_records.to_vec();
                move |tx| {
                    for access in access_records {
                        if receipt_record_matches_access(&record, &access) {
                            if access.retrieval_status != ReceiptRetrievalStatus::Retrieved {
                                tx.save_receipt_access_record(access.mark_retrieved(now));
                            }
                        } else if access.retrieval_status == ReceiptRetrievalStatus::Pending {
                            tx.save_receipt_access_record(access.mark_retrieval_error(
                                ReceiptRetrievalStatus::Failed,
                                now,
                                "Receipt Access does not match stored Receipt".into(),
                            ));
                        }
                    }
                    Ok(())
                }
            })
            .await
    }

    fn receipt_access_event_is_conflicted(
        tx: &dyn crate::storage::StorageTransaction,
        access: &ReceiptAccessRecord,
    ) -> bool {
        tx.event_dedup_record(&access.counterparty, &access.event_id)
            .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
    }

    fn receipt_record_access_event_is_conflicted(
        tx: &dyn crate::storage::StorageTransaction,
        record: &ReceiptRecord,
    ) -> bool {
        tx.event_dedup_record(&record.issuer, &record.receipt_access_event_id)
            .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
    }

    fn conflicted_receipt_access_error(receipt_id: &str) -> PaykitSdkError {
        PaykitSdkError::Protocol(format!(
            "Receipt Access event for receipt {receipt_id} has a conflicting Event ID"
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
