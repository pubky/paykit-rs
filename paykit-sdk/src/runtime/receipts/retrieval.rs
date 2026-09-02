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
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local_public_key = identity
            .public_key
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "no local Pubky identity available for receipt retrieval".into(),
                source: None,
            })?;
        self.ensure_peer_not_blocked(&counterparty).await?;
        let (stored_receipt, mut access_records, conflicted_access_records) = self
            .storage
            .transaction(|tx| {
                let stored_receipt = tx.receipt_record(&counterparty, receipt_id);
                let mut access_records = Vec::new();
                let mut conflicted_access_records = Vec::new();
                for record in tx
                    .receipt_access_records(&counterparty)
                    .into_iter()
                    .filter(|record| record.receipt_id == receipt_id)
                {
                    if Self::receipt_access_event_is_conflicted(tx, &record) {
                        conflicted_access_records.push(record);
                    } else {
                        access_records.push(record);
                    }
                }
                access_records.sort_by_key(|record| Reverse(record.stream_item_id));
                Ok((stored_receipt, access_records, conflicted_access_records))
            })
            .await?;
        if let Some(record) = stored_receipt {
            if record.recipient_public_key != local_public_key {
                return Err(PaykitSdkError::Protocol {
                    context: "stored Receipt recipient does not match local identity".into(),
                    source: None,
                });
            }
            access_records.retain(|access| access.app_authorized);
            if conflicted_access_records
                .iter()
                .any(|access| access.app_authorized)
            {
                return Err(Self::conflicted_receipt_access_error(receipt_id));
            }
            self.reconcile_cached_receipt_access_records(
                &record,
                &access_records,
                self.clock.now(),
            )
            .await?;
            self.storage
                .transaction({
                    let record = record.clone();
                    move |tx| Self::validate_receipt_record_accesses(tx, &record, None)
                })
                .await?;
            return Ok(record);
        }
        let authorized_app_ids = self.authorized_receipt_apps_for_peer(&counterparty).await?;
        self.persist_receipt_app_authorization(&counterparty, authorized_app_ids.as_deref())
            .await?;
        for record in &mut access_records {
            if authorized_app_ids
                .as_ref()
                .is_some_and(|app_ids| app_ids.contains(&record.app_id))
            {
                *record = record.mark_app_authorized();
            }
        }
        access_records.retain(|record| record.app_authorized);
        let conflicted_access_count = conflicted_access_records
            .into_iter()
            .filter(|record| {
                record.app_authorized
                    || authorized_app_ids
                        .as_ref()
                        .is_some_and(|app_ids| app_ids.contains(&record.app_id))
            })
            .count();
        if access_records.is_empty() && conflicted_access_count > 0 {
            return Err(Self::conflicted_receipt_access_error(receipt_id));
        }
        if access_records.is_empty() {
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no Receipt Access record for receipt {receipt_id} from {counterparty}"
                ),
                source: None,
            });
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
                    // The persisted record error may carry the Receipt
                    // Location: it stays in local storage, is redacted from
                    // record Debug output, and the record carries the location
                    // as a field anyway. The error returned to the caller must
                    // not: it crosses the FFI boundary, where its message is
                    // rendered verbatim into the generated Kotlin/Swift
                    // exception.
                    let error = format!(
                        "encrypted receipt {} was not found at {}",
                        access.receipt_id, access.location
                    );
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::NotFound,
                        now,
                        error,
                    )
                    .await?;
                    last_error = Some(merge_retrieval_error(
                        last_error.take(),
                        missing_encrypted_receipt_error(&access.location),
                    ));
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
                    let record = self
                        .storage
                        .transaction({
                            let access_event_id = access.event_id.clone();
                            let record = record.clone();
                            move |tx| {
                                let mut persisted_record = tx
                                    .receipt_record(&record.issuer, &record.receipt_id)
                                    .unwrap_or(record);
                                persisted_record.retrieved_at =
                                    persisted_record.retrieved_at.max(now);
                                Self::validate_receipt_record_accesses(
                                    tx,
                                    &persisted_record,
                                    Some(&access_event_id),
                                )?;
                                let current = tx
                                    .receipt_access_records(&persisted_record.issuer)
                                    .into_iter()
                                    .find(|candidate| candidate.event_id == access_event_id)
                                    .ok_or_else(|| PaykitSdkError::RecoveryRequired {
                                        context: format!(
                                            "Receipt Access event for receipt {} is no longer available",
                                            persisted_record.receipt_id
                                        ),
                                        source: None,
                                    })?;
                                tx.save_receipt_access_record(current.mark_retrieved(now));
                                tx.save_receipt_record(persisted_record.clone());
                                Ok(persisted_record)
                            }
                        })
                        .await?;
                    self.reconcile_cached_receipt_access_records(
                        &record,
                        &all_access_records,
                        record.retrieved_at,
                    )
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

        Err(
            last_error.unwrap_or_else(|| PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no usable Receipt Access record for receipt {receipt_id} from {counterparty}"
                ),
                source: None,
            }),
        )
    }

    /// List indexed Receipt Access records for one counterparty.
    pub async fn receipt_access_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<ReceiptAccessView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        let authorized_app_ids = self.authorized_receipt_apps_for_peer(counterparty).await?;
        self.persist_receipt_app_authorization(counterparty, authorized_app_ids.as_deref())
            .await?;
        self.storage
            .transaction(|tx| {
                let mut records = tx
                    .receipt_access_records(counterparty)
                    .into_iter()
                    .filter(|record| {
                        record.app_authorized
                            || authorized_app_ids
                                .as_ref()
                                .is_some_and(|app_ids| app_ids.contains(&record.app_id))
                    })
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
    ) -> Result<Vec<ReceiptAccessView>> {
        self.receipt_access_records(counterparty).await
    }

    /// List Receipt Access across non-blocked counterparties, newest first.
    pub async fn receipt_access(&self) -> Result<Vec<ReceiptAccessView>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        let counterparties = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .export_storage_state()
                    .receipt_access_records
                    .into_values()
                    .map(|record| record.counterparty)
                    .collect::<HashSet<_>>())
            })
            .await?;
        let mut authorized_by_counterparty = HashMap::new();
        for counterparty in counterparties {
            let authorized = self.authorized_receipt_apps_for_peer(&counterparty).await?;
            self.persist_receipt_app_authorization(&counterparty, authorized.as_deref())
                .await?;
            authorized_by_counterparty.insert(counterparty, authorized);
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
                            .get(&record.counterparty)
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .filter(|record| {
                        record.app_authorized
                            || authorized_by_counterparty
                                .get(&record.counterparty)
                                .and_then(Option::as_ref)
                                .is_some_and(|app_ids| app_ids.contains(&record.app_id))
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
    pub async fn receipt_records(&self, issuer: &PubkyPublicKey) -> Result<Vec<ReceiptRecord>> {
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
                    .filter(|record| Self::receipt_record_access_is_usable(tx, record))
                    .filter(|record| !Self::receipt_record_access_event_is_conflicted(tx, record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.retrieved_at));
                Ok(records)
            })
            .await
    }

    /// List decrypted receipts from one issuer, newest first.
    pub async fn receipts_from(&self, issuer: &PubkyPublicKey) -> Result<Vec<ReceiptRecord>> {
        self.receipt_records(issuer).await
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
                    .filter(|record| Self::receipt_record_access_is_usable(tx, record))
                    .filter(|record| {
                        !snapshot
                            .linked_peers
                            .get(&record.issuer)
                            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    })
                    .filter(|record| !Self::receipt_record_access_event_is_conflicted(tx, record))
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| Reverse(record.retrieved_at));
                Ok(records)
            })
            .await
    }

    async fn persist_receipt_app_authorization(
        &self,
        counterparty: &PubkyPublicKey,
        authorized_app_ids: Option<&[paykit_lib::PaykitAppId]>,
    ) -> Result<()> {
        let Some(authorized_app_ids) = authorized_app_ids else {
            return Ok(());
        };
        self.storage
            .transaction(|tx| {
                for record in tx.receipt_access_records(counterparty) {
                    if !record.app_authorized && authorized_app_ids.contains(&record.app_id) {
                        tx.save_receipt_access_record(record.mark_app_authorized());
                    }
                }
                Ok(())
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
                    for expected_access in access_records {
                        let Some(access) = tx
                            .receipt_access_records(&record.issuer)
                            .into_iter()
                            .find(|candidate| candidate.event_id == expected_access.event_id)
                        else {
                            continue;
                        };
                        if receipt_record_matches_access(&record, &access) {
                            if access.retrieval_status != ReceiptRetrievalStatus::Retrieved
                                || access
                                    .retrieved_at
                                    .is_none_or(|retrieved_at| retrieved_at < now)
                            {
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

    fn receipt_record_access_is_usable(
        tx: &dyn crate::storage::StorageTransaction,
        record: &ReceiptRecord,
    ) -> bool {
        Self::validate_receipt_record_accesses(tx, record, None).is_ok()
    }

    fn validate_receipt_record_accesses(
        tx: &dyn crate::storage::StorageTransaction,
        record: &ReceiptRecord,
        pending_event_id: Option<&str>,
    ) -> Result<()> {
        let access_records = tx
            .receipt_access_records(&record.issuer)
            .into_iter()
            .filter(|access| access.app_authorized && access.receipt_id == record.receipt_id)
            .collect::<Vec<_>>();
        if access_records
            .iter()
            .any(|access| Self::receipt_access_event_is_conflicted(tx, access))
        {
            return Err(Self::conflicted_receipt_access_error(&record.receipt_id));
        }
        if access_records
            .iter()
            .any(|access| !receipt_record_matches_access(record, access))
        {
            return Err(Self::mismatched_receipt_access_error(&record.receipt_id));
        }
        let has_provenance = access_records.iter().any(|access| {
            access.event_id == record.receipt_access_event_id
                && (access.retrieval_status == ReceiptRetrievalStatus::Retrieved
                    || pending_event_id == Some(access.event_id.as_str()))
        });
        if !has_provenance {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "stored Receipt {} has no authorized retrieved Receipt Access",
                    record.receipt_id
                ),
                source: None,
            });
        }
        Ok(())
    }

    fn conflicted_receipt_access_error(receipt_id: &str) -> PaykitSdkError {
        PaykitSdkError::Protocol {
            context: format!(
                "Receipt Access event for receipt {receipt_id} has a conflicting Event ID"
            ),
            source: None,
        }
    }

    fn mismatched_receipt_access_error(receipt_id: &str) -> PaykitSdkError {
        PaykitSdkError::Protocol {
            context: format!(
                "Receipt Access descriptor for receipt {receipt_id} conflicts with stored Receipt"
            ),
            source: None,
        }
    }

    pub(in crate::runtime) async fn save_receipt_retrieval_error(
        &self,
        access: &ReceiptAccessRecord,
        status: ReceiptRetrievalStatus,
        attempted_at: DateTime<Utc>,
        error: String,
    ) -> Result<()> {
        self.storage
            .transaction({
                let counterparty = access.counterparty.clone();
                let event_id = access.event_id.clone();
                move |tx| {
                    let Some(current) = tx
                        .receipt_access_records(&counterparty)
                        .into_iter()
                        .find(|candidate| candidate.event_id == event_id)
                    else {
                        return Ok(());
                    };
                    if current.retrieval_status != ReceiptRetrievalStatus::Retrieved {
                        tx.save_receipt_access_record(current.mark_retrieval_error(
                            status,
                            attempted_at,
                            error,
                        ));
                    }
                    Ok(())
                }
            })
            .await
    }
}
