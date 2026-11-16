//! THROWAWAY (#411): facade methods for the SDK flows round 1 never ran in a
//! browser: Payment Requests (accept, reject, cancel, conversion quote,
//! payment proof), Receipts and Receipt Access, Allowances and allowance
//! accounting, backup export/restore, Paykit identity key rotation, two
//! runtimes over one JS state store, and a call deadline.
//!
//! Every method mirrors a scenario from `paykit-sdk/tests/testnet_e2e` and
//! returns serde JSON of the SDK record so the JS driver can assert on it.
//! The records are test output only: some carry Payment References and proof
//! objects, which a real binding would not hand to JS as strings.

use std::{cell::Cell, sync::Arc, time::Duration};

use chrono::Utc;
use paykit_lib::{
    AllowanceAmountRange, AllowanceId, AllowanceTerms, BillingPeriod, ConversionRate, EventId,
    PaymentAmount, PaymentConversion, PaymentEndpointIdentifier, PaymentReference,
    PaymentRequestId, PaymentRequestTerms, Recurrence, RecurrenceConfig, RecurrenceUnit,
};
use paykit_sdk::{
    AllowanceAccountingReconciliation, AllowanceLocalRole, AllowanceSelectionInput, ContactUpdate,
    PaykitApp, PaykitAppCapabilities, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PaymentExecutionChecks, PaymentOccurrence, PaymentOutcome, PaymentOutcomeReport,
    PaymentProofSubmission, PaymentRequestScope, ReceiptDraftBuilder, RestoredLinkPolicy,
    SdkBackupState, StorageAdapter,
};
use serde::Serialize;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

use super::{js_err, make_storage, parse_key, JsStateStore, PaykitWasmClient, Sdk};

/// Error with the SDK error variant in brackets, so JS can assert on the kind
/// (for example `[ConcurrentUpdate]` or `[RecoveryRequired]`).
pub(super) fn sdk_err(context: &str, err: PaykitSdkError) -> JsError {
    let debug = format!("{err:?}");
    let kind = debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("Unknown");
    JsError::new(&format!("{context} [{kind}]: {err}"))
}

fn to_json<T: Serialize>(value: &T) -> Result<String, JsError> {
    serde_json::to_string(value).map_err(|e| js_err("serialize", e))
}

fn endpoint(id: &str) -> Result<PaymentEndpointIdentifier, JsError> {
    PaymentEndpointIdentifier::new(id).map_err(|e| js_err("endpoint identifier", e))
}

fn request_id(id: &str) -> Result<PaymentRequestId, JsError> {
    PaymentRequestId::new(id).map_err(|e| js_err("payment request id", e))
}

fn allowance_id(id: &str) -> Result<AllowanceId, JsError> {
    AllowanceId::new(id).map_err(|e| js_err("allowance id", e))
}

fn amount(value: &str, asset: &str) -> Result<PaymentAmount, JsError> {
    PaymentAmount::new(value, asset).map_err(|e| js_err("amount", e))
}

/// Wallet checks for a payment the wallet is about to make. THROWAWAY
/// (#411): the browser test asserts every check as true; a wallet derives
/// them from its own state and a trusted clock.
fn checks(endpoint_id: &str, value: &str, asset: &str) -> Result<PaymentExecutionChecks, JsError> {
    checks_at(Utc::now(), endpoint_id, value, asset)
}

fn checks_at(
    trusted_time: chrono::DateTime<Utc>,
    endpoint_id: &str,
    value: &str,
    asset: &str,
) -> Result<PaymentExecutionChecks, JsError> {
    Ok(PaymentExecutionChecks {
        trusted_time,
        payment_endpoint_identifier: endpoint(endpoint_id)?,
        actual_amount: amount(value, asset)?,
        endpoint_current: true,
        local_enabled: true,
        recurrence_eligible: true,
    })
}

/// SDK backup state kept inside wasm memory. The backup holds Noise key
/// material, so the facade never serializes it to a JS string.
#[wasm_bindgen]
pub struct PaykitWasmBackup {
    state: SdkBackupState,
}

#[wasm_bindgen]
impl PaykitWasmBackup {
    /// Content fingerprint of the backup.
    pub fn fingerprint(&self) -> Result<String, JsError> {
        self.state
            .content_fingerprint()
            .map_err(|e| sdk_err("content_fingerprint", e))
    }

    /// Record counts (no secret material).
    pub fn summary(&self) -> String {
        json!({
            "linkedPeers": self.state.linked_peers.len(),
            "privateStreamItems": self.state.private_stream_items.len(),
            "eventDedupRecords": self.state.event_dedup_records.len(),
        })
        .to_string()
    }
}

impl PaykitWasmClient {
    /// A new client value for the same identity, provider and adapter around
    /// another SDK runtime.
    pub(super) fn sibling(&self, sdk: Arc<Sdk>, storage_mode: &str) -> PaykitWasmClient {
        PaykitWasmClient {
            sdk,
            adapter: self.adapter.clone(),
            provider: self.provider.clone(),
            storage_mode: storage_mode.to_owned(),
            access: self.access.clone(),
            public_key: self.public_key.clone(),
            app_id: self.app_id.clone(),
            intake: None,
            deadline_ms: Cell::new(None),
        }
    }

    fn new_sdk(&self, storage: Box<dyn StorageAdapter>) -> Result<Arc<Sdk>, JsError> {
        let config = PaykitSdkConfig::new(self.app_id.clone()).map_err(|e| js_err("config", e))?;
        Ok(Arc::new(PaykitSdk::new(
            storage,
            self.provider.clone(),
            self.adapter.clone(),
            config,
        )))
    }

    /// Run `fut` under the facade call deadline when one is set.
    ///
    /// THROWAWAY (#411): stopgap for pubky's `request_timeout` being a no-op on
    /// wasm. Dropping the future drops reqwest's wasm `AbortGuard`, which
    /// calls `AbortController.abort()`, so the in-flight fetch is cancelled
    /// rather than leaked. It is cancellation at an arbitrary await point:
    /// an SDK write may or may not have committed (same contract as a native
    /// caller-side timeout). Paykit's documented policy is that it enforces no
    /// deadline itself; this lives in the binding layer, not in the SDK.
    pub(super) async fn with_deadline<T>(
        &self,
        what: &str,
        fut: impl std::future::Future<Output = Result<T, JsError>>,
    ) -> Result<T, JsError> {
        match self.deadline_ms.get() {
            None => fut.await,
            Some(ms) => paykit_lib::rt::timeout(Duration::from_millis(ms.into()), fut)
                .await
                .unwrap_or_else(|_| {
                    Err(JsError::new(&format!(
                        "{what} [DeadlineExceeded]: no result within {ms} ms"
                    )))
                }),
        }
    }
}

#[wasm_bindgen]
impl PaykitWasmClient {
    // -----------------------------------------------------------------------
    // Delivery and deadlines
    // -----------------------------------------------------------------------

    /// Set (or clear with `undefined`) the facade-level call deadline.
    #[wasm_bindgen(js_name = setCallDeadlineMs)]
    pub fn set_call_deadline_ms(&self, ms: Option<u32>) {
        self.deadline_ms.set(ms);
    }

    /// `process_outbound_private_messages(peer)`; returns `{sent, failed}`.
    #[wasm_bindgen(js_name = deliverTo)]
    pub async fn deliver_to(&self, peer: String) -> Result<String, JsError> {
        let report = self
            .sdk
            .process_outbound_private_messages(parse_key(&peer)?)
            .await
            .map_err(|e| sdk_err("process_outbound_private_messages", e))?;
        Ok(json!({ "sent": report.sent.len(), "failed": report.failed.len() }).to_string())
    }

    /// `receive_private_messages(peer)`; returns `{items, conflicts}` or an
    /// error with the SDK error kind.
    #[wasm_bindgen(js_name = receiveFrom)]
    pub async fn receive_from(&self, peer: String) -> Result<String, JsError> {
        let report = self
            .sdk
            .receive_private_messages(parse_key(&peer)?)
            .await
            .map_err(|e| sdk_err("receive_private_messages", e))?;
        Ok(json!({
            "items": report.stream_item_ids.len(),
            "conflicts": report.event_conflicts.len(),
        })
        .to_string())
    }

    // -----------------------------------------------------------------------
    // Payment Requests
    // -----------------------------------------------------------------------

    /// Payee side: queue a Payment Request to `payer`. With
    /// `recurrence_starts_at` (RFC3339) it is a monthly request with
    /// per-period conversion, as in `payment_conversion.rs`.
    #[wasm_bindgen(js_name = proposePaymentRequest)]
    pub async fn propose_payment_request(
        &self,
        payer: String,
        value: String,
        asset: String,
        reference: String,
        endpoint_id: String,
        recurrence_starts_at: Option<String>,
    ) -> Result<String, JsError> {
        let recurrence = recurrence_starts_at
            .map(|starts_at| {
                Recurrence::try_from(RecurrenceConfig {
                    every: 1,
                    unit: RecurrenceUnit::Month,
                    starts_at: starts_at.clone(),
                    anchor: starts_at,
                    ends_at: None,
                })
                .map_err(|e| js_err("recurrence", e))
            })
            .transpose()?;
        let conversion = recurrence.as_ref().map(|_| PaymentConversion::PerPeriod {});
        let terms = PaymentRequestTerms::builder(
            amount(&value, &asset)?,
            PaymentReference::new(reference).map_err(|e| js_err("reference", e))?,
            vec![endpoint(&endpoint_id)?],
        )
        .recurrence(recurrence)
        .conversion(conversion)
        .build()
        .map_err(|e| js_err("terms", e))?;
        let record = self
            .sdk
            .propose_payment_request(parse_key(&payer)?, terms)
            .await
            .map_err(|e| sdk_err("propose_payment_request", e))?;
        to_json(&record)
    }

    /// All Payment Request records with `peer`, as a JSON array.
    #[wasm_bindgen(js_name = paymentRequestsWith)]
    pub async fn payment_requests_with(&self, peer: String) -> Result<String, JsError> {
        let records = self
            .sdk
            .payment_requests_with(&parse_key(&peer)?)
            .await
            .map_err(|e| sdk_err("payment_requests_with", e))?;
        to_json(&records)
    }

    /// Payer side: claim the received request for execution only.
    #[wasm_bindgen(js_name = claimPaymentRequest)]
    pub async fn claim_payment_request(&self, payee: String, id: String) -> Result<(), JsError> {
        self.sdk
            .claim_payment_request_for_execution(parse_key(&payee)?, &request_id(&id)?)
            .await
            .map_err(|e| sdk_err("claim_payment_request_for_execution", e))?;
        Ok(())
    }

    /// Payer side: claim the received request for execution and accept the
    /// proposal this runtime currently holds.
    #[wasm_bindgen(js_name = claimAndAcceptPaymentRequest)]
    pub async fn claim_and_accept_payment_request(
        &self,
        payee: String,
        id: String,
    ) -> Result<String, JsError> {
        let payee = parse_key(&payee)?;
        let id = request_id(&id)?;
        self.sdk
            .claim_payment_request_for_execution(payee.clone(), &id)
            .await
            .map_err(|e| sdk_err("claim_payment_request_for_execution", e))?;
        let item = self
            .sdk
            .payment_requests_with(&payee)
            .await
            .map_err(|e| sdk_err("payment_requests_with", e))?
            .into_iter()
            .find(|record| record.payment_request_id == id.as_str())
            .and_then(|record| record.proposal_stream_item_id)
            .ok_or_else(|| JsError::new("no received proposal to accept"))?;
        let record = self
            .sdk
            .accept_payment_request(payee, &id, item)
            .await
            .map_err(|e| sdk_err("accept_payment_request", e))?;
        to_json(&record)
    }

    /// Payer side: reject a received request.
    #[wasm_bindgen(js_name = rejectPaymentRequest)]
    pub async fn reject_payment_request(
        &self,
        payee: String,
        id: String,
        reason: Option<String>,
    ) -> Result<String, JsError> {
        let record = self
            .sdk
            .reject_payment_request(parse_key(&payee)?, &request_id(&id)?, reason)
            .await
            .map_err(|e| sdk_err("reject_payment_request", e))?;
        to_json(&record)
    }

    /// Either side: cancel a request.
    #[wasm_bindgen(js_name = cancelPaymentRequest)]
    pub async fn cancel_payment_request(
        &self,
        peer: String,
        id: String,
        reason: Option<String>,
    ) -> Result<String, JsError> {
        let record = self
            .sdk
            .cancel_payment_request(parse_key(&peer)?, &request_id(&id)?, reason)
            .await
            .map_err(|e| sdk_err("cancel_payment_request", e))?;
        to_json(&record)
    }

    /// Payee side: issue a conversion quote for one Billing Period.
    #[wasm_bindgen(js_name = quotePaymentRequest)]
    #[allow(clippy::too_many_arguments)]
    pub async fn quote_payment_request(
        &self,
        payer: String,
        id: String,
        period_start: String,
        period_end: String,
        rate_asset: String,
        rate_value: String,
        expires_at: String,
    ) -> Result<String, JsError> {
        let period =
            BillingPeriod::new(period_start, period_end).map_err(|e| js_err("period", e))?;
        let record = self
            .sdk
            .quote_payment_request(
                parse_key(&payer)?,
                &request_id(&id)?,
                period,
                vec![ConversionRate {
                    asset: rate_asset,
                    value: rate_value,
                }],
                expires_at,
            )
            .await
            .map_err(|e| sdk_err("quote_payment_request", e))?;
        to_json(&record)
    }

    /// Payer side: submit a Payment Proof (`proof_json` is a JSON object).
    #[wasm_bindgen(js_name = submitPaymentProof)]
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_payment_proof(
        &self,
        payee: String,
        id: String,
        payment_app_id: String,
        endpoint_id: String,
        period_start: Option<String>,
        period_end: Option<String>,
        conversion_quote_id: Option<String>,
        allowance: Option<String>,
        proof_json: String,
    ) -> Result<String, JsError> {
        let billing_period = match (period_start, period_end) {
            (Some(start), Some(end)) => {
                Some(BillingPeriod::new(start, end).map_err(|e| js_err("period", e))?)
            }
            _ => None,
        };
        let proof = match serde_json::from_str::<Value>(&proof_json) {
            Ok(Value::Object(map)) => map,
            _ => return Err(JsError::new("proof must be a JSON object")),
        };
        let submission = PaymentProofSubmission {
            payment_app_id: paykit_lib::PaykitAppId::new(payment_app_id)
                .map_err(|e| js_err("payment app id", e))?,
            billing_period,
            payment_endpoint_identifier: endpoint(&endpoint_id)?,
            conversion_quote_id: conversion_quote_id
                .map(|id| EventId::new(&id).map_err(|e| js_err("quote id", e)))
                .transpose()?,
            allowance_id: allowance.map(|id| allowance_id(&id)).transpose()?,
            proof,
        };
        let record = self
            .sdk
            .submit_payment_proof_submission(parse_key(&payee)?, &request_id(&id)?, submission)
            .await
            .map_err(|e| sdk_err("submit_payment_proof_submission", e))?;
        to_json(&record)
    }

    // -----------------------------------------------------------------------
    // Receipts and Receipt Access
    // -----------------------------------------------------------------------

    /// Issuer side: store an encrypted Receipt and queue Receipt Access.
    #[wasm_bindgen(js_name = issueReceipt)]
    pub async fn issue_receipt(
        &self,
        recipient: String,
        reference: String,
        value: String,
        asset: String,
    ) -> Result<String, JsError> {
        let draft = ReceiptDraftBuilder::from_payment_reference(
            PaymentReference::new(reference).map_err(|e| js_err("reference", e))?,
        )
        .with_new_receipt_id()
        .with_amount_text(value, asset)
        .map_err(|e| sdk_err("receipt amount", e))?
        .build()
        .map_err(|e| sdk_err("receipt draft", e))?;
        let view = self
            .sdk
            .issue_receipt(parse_key(&recipient)?, draft)
            .await
            .map_err(|e| sdk_err("issue_receipt", e))?;
        to_json(&view)
    }

    /// Issuer side: issued receipt views for `recipient`.
    #[wasm_bindgen(js_name = issuedReceiptsTo)]
    pub async fn issued_receipts_to(&self, recipient: String) -> Result<String, JsError> {
        let views = self
            .sdk
            .issued_receipts_to(&parse_key(&recipient)?)
            .await
            .map_err(|e| sdk_err("issued_receipts_to", e))?;
        to_json(&views)
    }

    /// Recipient side: fetch the encrypted Receipt from the issuer's
    /// homeserver and decrypt it with the received Receipt Access key.
    #[wasm_bindgen(js_name = retrieveReceipt)]
    pub async fn retrieve_receipt(
        &self,
        issuer: String,
        receipt_id: String,
    ) -> Result<String, JsError> {
        let record = self
            .sdk
            .retrieve_receipt(parse_key(&issuer)?, &receipt_id)
            .await
            .map_err(|e| sdk_err("retrieve_receipt", e))?;
        to_json(&record)
    }

    // -----------------------------------------------------------------------
    // Allowances and allowance accounting
    // -----------------------------------------------------------------------

    /// Queue an Allowance proposal (`role` is `"allower"` or `"allowee"`).
    #[wasm_bindgen(js_name = proposeAllowance)]
    pub async fn propose_allowance(
        &self,
        peer: String,
        role: String,
        asset: String,
        per_payment_min: String,
        per_payment_max: String,
        lifetime_limit: String,
    ) -> Result<String, JsError> {
        let role = match role.as_str() {
            "allower" => AllowanceLocalRole::Allower,
            "allowee" => AllowanceLocalRole::Allowee,
            other => return Err(JsError::new(&format!("unknown role {other:?}"))),
        };
        let terms = AllowanceTerms::builder(asset)
            .per_payment_amount(
                AllowanceAmountRange::new(per_payment_min, per_payment_max)
                    .map_err(|e| js_err("amount range", e))?,
            )
            .lifetime_amount_limit(lifetime_limit)
            .build()
            .map_err(|e| js_err("allowance terms", e))?;
        let record = self
            .sdk
            .propose_allowance(parse_key(&peer)?, role, terms)
            .await
            .map_err(|e| sdk_err("propose_allowance", e))?;
        to_json(&record)
    }

    /// Allowance record on the link with `peer`, or `null`.
    #[wasm_bindgen(js_name = allowanceRecord)]
    pub async fn allowance_record(&self, peer: String, id: String) -> Result<String, JsError> {
        let record = self
            .sdk
            .allowance_record(&parse_key(&peer)?, &allowance_id(&id)?)
            .await
            .map_err(|e| sdk_err("allowance_record", e))?;
        to_json(&record)
    }

    /// Accept the Allowance proposal this runtime currently holds.
    #[wasm_bindgen(js_name = acceptAllowance)]
    pub async fn accept_allowance(&self, peer: String, id: String) -> Result<String, JsError> {
        let peer = parse_key(&peer)?;
        let id = allowance_id(&id)?;
        let item = self
            .sdk
            .allowance_record(&peer, &id)
            .await
            .map_err(|e| sdk_err("allowance_record", e))?
            .and_then(|record| record.proposal_stream_item_id)
            .ok_or_else(|| JsError::new("no received Allowance proposal"))?;
        let record = self
            .sdk
            .accept_allowance(peer, &id, item)
            .await
            .map_err(|e| sdk_err("accept_allowance", e))?;
        to_json(&record)
    }

    /// Make allowance accounting usable: when no ledger exists yet, run the
    /// first reconciliation (as the native accounting test does); when one
    /// exists and needs reconciliation, reconcile it at its current revision
    /// with no new outcomes; otherwise leave it alone. Returns
    /// `{action, state}`.
    ///
    /// THROWAWAY (#411): a wallet must supply its real outcomes here; the
    /// spike has made no payments outside this page.
    #[wasm_bindgen(js_name = initializeAllowanceAccounting)]
    pub async fn initialize_allowance_accounting(&self) -> Result<String, JsError> {
        let existing = self
            .sdk
            .allowance_accounting_state()
            .await
            .map_err(|e| sdk_err("allowance_accounting_state", e))?;
        let (action, input) = match existing {
            Some(state) if !state.requires_reconciliation => {
                return Ok(json!({ "action": "existing", "state": state }).to_string())
            }
            Some(state) => (
                "reconciled",
                AllowanceAccountingReconciliation {
                    expected_revision: Some(state.revision),
                    history: state.history,
                    outcomes: vec![],
                    trusted_time: Utc::now(),
                },
            ),
            None => (
                "created",
                AllowanceAccountingReconciliation {
                    expected_revision: None,
                    history: Default::default(),
                    outcomes: vec![],
                    trusted_time: Utc::now(),
                },
            ),
        };
        let state = self
            .sdk
            .reconcile_allowance_accounting(input)
            .await
            .map_err(|e| sdk_err("reconcile_allowance_accounting", e))?;
        Ok(json!({ "action": action, "state": state }).to_string())
    }

    /// Payer side: evaluate candidates, select `allowance` for the request
    /// and accept it automatically. Returns `{candidates, association}`.
    #[wasm_bindgen(js_name = selectAllowanceAndAccept)]
    pub async fn select_allowance_and_accept(
        &self,
        payee: String,
        id: String,
        allowance: String,
        endpoint_id: String,
        value: String,
        asset: String,
    ) -> Result<String, JsError> {
        let scope = PaymentRequestScope {
            counterparty: parse_key(&payee)?,
            payment_request_id: request_id(&id)?,
        };
        let trusted_time = Utc::now();
        let candidates = self
            .sdk
            .evaluate_allowance_candidates(scope.clone(), trusted_time)
            .await
            .map_err(|e| sdk_err("evaluate_allowance_candidates", e))?;
        let selection = AllowanceSelectionInput {
            allowance_id: allowance_id(&allowance)?,
            expected_revision: None,
            trusted_time,
        };
        let association = self
            .sdk
            .select_allowance(scope.clone(), selection.clone())
            .await
            .map_err(|e| sdk_err("select_allowance", e))?;
        let revision = association
            .revisions
            .last()
            .map(|r| r.revision)
            .ok_or_else(|| JsError::new("selection has no revision"))?;
        let accepted = self
            .sdk
            .accept_payment_request_automatically(
                scope,
                AllowanceSelectionInput {
                    expected_revision: Some(revision),
                    ..selection
                },
                // Automatic Acceptance requires the selection's trusted time.
                checks_at(trusted_time, &endpoint_id, &value, &asset)?,
            )
            .await
            .map_err(|e| sdk_err("accept_payment_request_automatically", e))?;
        Ok(json!({
            "candidates": candidates.iter().map(|c| json!({
                "allowanceId": c.allowance_id,
                "blocked": c.blocked.as_ref().map(|b| format!("{b:?}")),
            })).collect::<Vec<_>>(),
            "revision": revision,
            "association": serde_json::to_value(&accepted).map_err(|e| js_err("serialize", e))?,
        })
        .to_string())
    }

    /// Payer side: reserve allowance capacity for a one-time request.
    #[wasm_bindgen(js_name = reserveAutomaticPayment)]
    pub async fn reserve_automatic_payment(
        &self,
        payee: String,
        id: String,
        association_revision: u32,
        endpoint_id: String,
        value: String,
        asset: String,
    ) -> Result<String, JsError> {
        let occurrence = PaymentOccurrence {
            request: PaymentRequestScope {
                counterparty: parse_key(&payee)?,
                payment_request_id: request_id(&id)?,
            },
            billing_period: None,
        };
        let decision = self
            .sdk
            .reserve_automatic_payment(
                occurrence,
                association_revision.into(),
                checks(&endpoint_id, &value, &asset)?,
            )
            .await
            .map_err(|e| sdk_err("reserve_automatic_payment", e))?;
        to_json(&decision)
    }

    /// Payer side: hand a Prepared attempt to the payment rail.
    #[wasm_bindgen(js_name = beginPaymentExecution)]
    pub async fn begin_payment_execution(
        &self,
        attempt_id: String,
        endpoint_id: String,
        value: String,
        asset: String,
    ) -> Result<String, JsError> {
        let decision = self
            .sdk
            .begin_payment_execution(attempt_id, checks(&endpoint_id, &value, &asset)?)
            .await
            .map_err(|e| sdk_err("begin_payment_execution", e))?;
        to_json(&decision)
    }

    /// Payer side: record the wallet's attested outcome for an attempt.
    #[wasm_bindgen(js_name = recordPaymentOutcome)]
    pub async fn record_payment_outcome(
        &self,
        attempt_id: String,
        outcome: String,
    ) -> Result<String, JsError> {
        let outcome = match outcome.as_str() {
            "succeeded" => PaymentOutcome::Succeeded,
            "failed" => PaymentOutcome::Failed,
            "unknown" => PaymentOutcome::Unknown,
            other => return Err(JsError::new(&format!("unknown outcome {other:?}"))),
        };
        let record = self
            .sdk
            .record_payment_outcome(PaymentOutcomeReport {
                attempt_id,
                outcome,
            })
            .await
            .map_err(|e| sdk_err("record_payment_outcome", e))?;
        to_json(&record)
    }

    /// Allowance accounting state as JSON (or `null`).
    #[wasm_bindgen(js_name = allowanceAccountingState)]
    pub async fn allowance_accounting_state(&self) -> Result<String, JsError> {
        let state = self
            .sdk
            .allowance_accounting_state()
            .await
            .map_err(|e| sdk_err("allowance_accounting_state", e))?;
        to_json(&state)
    }

    // -----------------------------------------------------------------------
    // Backup and restore
    // -----------------------------------------------------------------------

    /// Export the SDK backup state into an opaque wasm object.
    #[wasm_bindgen(js_name = exportBackup)]
    pub async fn export_backup(&self) -> Result<PaykitWasmBackup, JsError> {
        let state = self
            .sdk
            .export_backup_state()
            .await
            .map_err(|e| sdk_err("export_backup_state", e))?;
        Ok(PaykitWasmBackup { state })
    }

    /// Build a fresh runtime for this identity over new storage, initialize
    /// it, restore `backup` (consumed) and republish the Paykit app, in the
    /// order `restore_backup_state` documents. `resume` selects
    /// `RestoredLinkPolicy::Resume`; otherwise links require recovery.
    ///
    /// Returns the restored client. The caller must stop using the runtime
    /// the backup came from: with `Resume`, two runtimes sending on the same
    /// Encrypted Link would reuse a transport key and nonce.
    #[wasm_bindgen(js_name = restoreIntoFreshRuntime)]
    pub async fn restore_into_fresh_runtime(
        &self,
        backup: PaykitWasmBackup,
        storage_mode: String,
        store: Option<JsStateStore>,
        resume: bool,
    ) -> Result<PaykitWasmClient, JsError> {
        let storage = make_storage(&storage_mode, store, &self.provider)?;
        let sdk = self.new_sdk(storage)?;
        sdk.initialize()
            .await
            .map_err(|e| sdk_err("initialize (restore target)", e))?;
        let policy = if resume {
            RestoredLinkPolicy::Resume
        } else {
            RestoredLinkPolicy::RequireRecovery
        };
        let report = sdk
            .restore_backup_state(backup.state, policy)
            .await
            .map_err(|e| sdk_err("restore_backup_state", e))?;
        if !report.recovery_required_peers.is_empty() && resume {
            return Err(JsError::new(&format!(
                "restore with Resume left {} peers needing recovery",
                report.recovery_required_peers.len()
            )));
        }
        sdk.publish_paykit_app(
            PaykitApp::new(
                "Paykit wasm spike",
                PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            )
            .map_err(|e| js_err("PaykitApp::new", e))?,
        )
        .await
        .map_err(|e| sdk_err("publish_paykit_app (restored)", e))?;
        Ok(self.sibling(sdk, &storage_mode))
    }

    // -----------------------------------------------------------------------
    // Two runtimes over one store (CAS)
    // -----------------------------------------------------------------------

    /// A second SDK runtime for this identity over a separate `JsStateStore`
    /// object (which may point at the same IndexedDB database), initialized.
    /// Its Rust-side transaction mutex is its own, like a second tab's.
    #[wasm_bindgen(js_name = openRuntimeOnStore)]
    pub async fn open_runtime_on_store(
        &self,
        store: JsStateStore,
    ) -> Result<PaykitWasmClient, JsError> {
        let storage = make_storage("js", Some(store), &self.provider)?;
        let sdk = self.new_sdk(storage)?;
        sdk.initialize()
            .await
            .map_err(|e| sdk_err("initialize (second runtime)", e))?;
        Ok(self.sibling(sdk, "js"))
    }

    /// `save_contact` (one storage transaction, no SDK retry), so a
    /// compare-and-swap loss surfaces as `[ConcurrentUpdate]`.
    #[wasm_bindgen(js_name = saveContact)]
    pub async fn save_contact(&self, contact: String, label: String) -> Result<String, JsError> {
        let record = self
            .sdk
            .save_contact(ContactUpdate {
                public_key: parse_key(&contact)?,
                label: Some(label),
            })
            .await
            .map_err(|e| sdk_err("save_contact", e))?;
        Ok(record.public_key.as_str().to_owned())
    }

    /// Public keys of all Contact Records, as a JSON array.
    #[wasm_bindgen(js_name = contactKeys)]
    pub async fn contact_keys(&self) -> Result<String, JsError> {
        let records = self
            .sdk
            .contact_records()
            .await
            .map_err(|e| sdk_err("contact_records", e))?;
        let keys: Vec<&str> = records.iter().map(|r| r.public_key.as_str()).collect();
        to_json(&keys)
    }

    // -----------------------------------------------------------------------
    // Key rotation and link recovery
    // -----------------------------------------------------------------------

    /// Rotate the identity-wide Paykit key to the next generation, derived
    /// from the local Pubky secret, then hand the replacement to the session
    /// provider (the "distribute the replacement" step) so every runtime of
    /// this identity in this page uses it.
    ///
    /// THROWAWAY (#411): the replacement key only lives in wasm memory. A real
    /// browser app must persist it before calling (the SDK docs require it),
    /// which is the open "where do browser secrets live" decision.
    #[wasm_bindgen(js_name = rotatePaykitIdentityKey)]
    pub async fn rotate_paykit_identity_key(&self) -> Result<String, JsError> {
        let access = self
            .provider
            .access
            .lock()
            .expect("session lock")
            .clone()
            .ok_or_else(|| JsError::new("no session"))?;
        let local = access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| JsError::new("rotation needs the local Pubky secret"))?;
        let current = access
            .paykit_identity_secret_key
            .as_ref()
            .map(|key| key.key_generation())
            .unwrap_or(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION);
        let replacement = local
            .derive_paykit_identity_secret_key(current + 1)
            .map_err(|e| sdk_err("derive replacement", e))?;
        let registry = self
            .sdk
            .rotate_paykit_identity_key(replacement.clone())
            .await
            .map_err(|e| sdk_err("rotate_paykit_identity_key", e))?;
        if let Some(access) = self.provider.access.lock().expect("session lock").as_mut() {
            access.paykit_identity_secret_key = Some(replacement);
        }
        Ok(json!({
            "fromGeneration": current,
            "toGeneration": current + 1,
            "registryKeyGeneration": registry.key_generation(),
        })
        .to_string())
    }

    /// `ensure_link_with_peer(peer, 1)`: one recovery or handshake step.
    #[wasm_bindgen(js_name = ensureLink)]
    pub async fn ensure_link(&self, peer: String) -> Result<String, JsError> {
        let report = self
            .sdk
            .ensure_link_with_peer(parse_key(&peer)?, 1)
            .await
            .map_err(|e| sdk_err("ensure_link_with_peer", e))?;
        Ok(format!("{:?}", report.state))
    }
}
