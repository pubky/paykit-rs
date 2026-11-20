//! Receipts with the payee's members, and the payer's view of the receipts it holds
//! (Paykit spec 5, Paykit wire 4.11, 5.3).

use paykit_lib::periods::PeriodRef;
use paykit_lib::PaymentReference;
use paykit_sdk::{
    PayOptions, PayOutcome, PayeeStatus, ReceiptDetails, ReceiptsOf, RequestRef, UnrequestedPayment,
};
use pubky_switch_client::clock::Clock;
use serde_json::json;

use crate::common::{btc, draft, endpoint, list_version, request_from, World};

fn metadata() -> serde_json::Map<String, serde_json::Value> {
    json!({ "order": "o-7", "note": "thanks" })
        .as_object()
        .cloned()
        .unwrap()
}

/// A signed payer proof available to the test before the reader sees it in shared data.
pub(crate) fn requested_proof(
    w: &World,
    payer: &paykit_sdk::Paykit,
    shop: &paykit_sdk::Paykit,
    request: &RequestRef,
    attempt: paykit_lib::AttemptId,
) -> paykit_lib::records::StoredMessage {
    use pubky_switch_client::app::OutgoingMessage;
    let body = json!({
        "attempt_id": attempt.to_text(),
        "payment_endpoint_identifier": "btc-lightning-bolt11",
        "payment_app_id": pubky_switch_core::enc::zbase32_encode(&shop.app_id()),
        "proof": { "reference": "purchase" }
    });
    let mut message = OutgoingMessage::new(
        shop.identity(),
        "paykit.payment_proof",
        body.as_object().unwrap().clone(),
    );
    message.refs.push(request.request_hash);
    let signed = payer.app().sign_message(message).unwrap();
    paykit_lib::records::StoredMessage::new(
        payer.identity(),
        payer.app().cert().signed(),
        signed.plaintext().clone(),
        Some(w.clock.now_ms()),
        &shop.identity(),
    )
    .unwrap()
}

/// Publish the proof after this wallet has observed the original signed request.
pub(crate) async fn publish_requested_proof(
    payer: &mut paykit_sdk::Paykit,
    request: &RequestRef,
    proof: paykit_lib::records::StoredMessage,
) {
    let target = crate::outcome_record::payer_record(request);
    let held = payer.app_mut().read_record(&target).await.unwrap().unwrap();
    let value = record_with_proof(held.content.value().unwrap(), payer, proof);
    crate::outcome_record::phone_writes(payer, request, Some(&value)).await;
}

pub(crate) fn record_with_proof(
    value: &[u8],
    payer: &paykit_sdk::Paykit,
    proof: paykit_lib::records::StoredMessage,
) -> Vec<u8> {
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    let mut record = ActivityRecord::read(value, &payer.identity(), ActivitySide::Payer).unwrap();
    record.proof = Some(proof);
    record.to_json().unwrap()
}

/// The payer has the signed proof when the receipt reaches its inbox.
pub(crate) async fn deliver_proved_receipt(
    w: &World,
    payer: &mut paykit_sdk::Paykit,
    shop: &paykit_sdk::Paykit,
    request: &RequestRef,
    attempt: paykit_lib::AttemptId,
) {
    let proof = requested_proof(w, payer, shop, request, attempt);
    let receipt = signed_receipt(w, shop, request, &proof);
    publish_requested_proof(payer, request, proof).await;
    w.deliver(
        &w.bob,
        "shop",
        &w.alice,
        &[payer.app_id()],
        receipt.plaintext(),
    );
}

/// Bob's receipt for the payment Alice's signed proof identifies.
pub(crate) fn signed_receipt(
    w: &World,
    shop: &paykit_sdk::Paykit,
    request: &RequestRef,
    proof: &paykit_lib::records::StoredMessage,
) -> paykit_lib::records::StoredMessage {
    let paykit_lib::Body::PaymentProof(payment) = proof.body() else {
        panic!("expected a signed payment proof");
    };
    let receipt = paykit_lib::bodies::PaymentReceipt {
        attempt_id: payment.attempt_id,
        billing_period: payment.billing_period,
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
    .unwrap();
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_receipt",
        vec![proof.hash(), request.request_hash],
        receipt,
    );
    paykit_lib::records::StoredMessage::new(
        w.bob_id(),
        shop.app().cert().signed(),
        plaintext,
        Some(w.clock.now_ms()),
        &w.alice_id(),
    )
    .unwrap()
}

#[tokio::test]
async fn test_receipts_one_time_credit_is_shown_once_the_payee_holds_the_signed_proof() {
    use crate::periods::{payee_request_at, writes_value};
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let payer_key = wallet.request(&request).await.unwrap().record;
    let payer = wallet
        .app()
        .shared_record(&payer_key)
        .await
        .unwrap()
        .unwrap();
    let proof = ActivityRecord::read(
        payer.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap()
    .proof
    .unwrap();
    let target = payee_request_at(&request);
    let key = shop.app().record_key(&target).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let mut record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    let initial = shop.payee_activity().await.unwrap().remove(0);
    record.receipt = Some(signed_receipt(&w, &shop, &request, &proof));
    writes_value(&mut shop, &target, &record.to_json().unwrap()).await;
    let before = shop.payee_activity().await.unwrap().remove(0);
    assert_eq!(before.status, initial.status);
    assert!(before.attempt_id.is_none());
    record.proof = Some(proof);
    writes_value(&mut shop, &target, &record.to_json().unwrap()).await;
    let after = shop.payee_activity().await.unwrap().remove(0);
    assert_eq!(after.status, PayeeStatus::Receipted);
    assert_eq!(after.attempt_id, Some(outcome.attempt_id()));
}

#[tokio::test]
async fn test_receipts_carry_metadata_and_are_listed_by_payee_request_and_period() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let attempt_id = outcome.attempt_id();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let msg_id = shop
        .confirm_payment_with(&record, ReceiptDetails::new().with_metadata(metadata()))
        .await
        .unwrap()
        .unwrap();
    wallet.receive(None).await.unwrap();
    let receipt = wallet.request(&request).await.unwrap().receipt.unwrap();
    assert_eq!(receipt.msg_id, msg_id);
    assert_eq!(receipt.attempt_id, attempt_id);
    assert_eq!(receipt.payee, w.bob_id());
    assert_eq!(receipt.request_hash, Some(request.request_hash));
    assert_eq!(receipt.metadata, Some(metadata()));
    // The request's reference.
    assert_eq!(receipt.payment_reference.as_deref(), Some("order-1"));
    assert_eq!(
        wallet.payments().await.unwrap()[0].receipt,
        Some(receipt.clone())
    );
    let other = RequestRef {
        payee: w.bob_id(),
        request_hash: [9; 32],
    };
    let period = PeriodRef {
        starts_at: 1,
        ends_at: 2,
    };
    for (of, listed) in [
        (ReceiptsOf::All, 1),
        (ReceiptsOf::Payee(w.bob_id()), 1),
        (ReceiptsOf::Payee(w.alice_id()), 0),
        (ReceiptsOf::Request(request), 1),
        (ReceiptsOf::Request(other), 0),
        (ReceiptsOf::Period(request, period), 0),
        (
            ReceiptsOf::Reference(PaymentReference::new("order-1").unwrap()),
            1,
        ),
        (
            ReceiptsOf::Reference(PaymentReference::new("order-2").unwrap()),
            0,
        ),
    ] {
        let listed_now = wallet.receipts(of.clone()).await.unwrap().len();
        assert_eq!(listed_now, listed, "{of:?}");
    }
}

#[tokio::test]
async fn test_receipts_of_a_request_carry_no_payment_reference() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let details =
        ReceiptDetails::new().with_payment_reference(PaymentReference::new("inv-7").unwrap());
    assert!(shop.confirm_payment_with(&record, details).await.is_err());
    // Nothing went out.
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().receipt.is_none());
}

#[tokio::test]
async fn test_receipts_of_a_payment_without_a_request_carry_the_payees_reference() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.001")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let details =
        ReceiptDetails::new().with_payment_reference(PaymentReference::new("inv-7").unwrap());
    shop.confirm_payment_with(&record, details)
        .await
        .unwrap()
        .unwrap();
    wallet.receive(None).await.unwrap();
    let receipts = wallet.receipts(ReceiptsOf::All).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].payment_reference.as_deref(), Some("inv-7"));
    assert_eq!(receipts[0].request_hash, None);
    let by_reference = wallet
        .receipts(ReceiptsOf::Reference(
            PaymentReference::new("inv-7").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(by_reference.len(), 1);
}

#[tokio::test]
async fn test_receipts_of_a_billing_period_carry_the_requests_reference() {
    // Another own app wrote alice's period record of bob's monthly request, holding bob's
    // receipt of the first Billing Period. The period record holds no request: the
    // receipt's reference is the request's, read from the request record.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "plan-7",
        "proposal_expires_at": pubky_switch_core::time::rfc3339(w.clock.now_ms() + 3_600_000),
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" },
        "recurrence": {
            "anchor": "2026-01-31T00:00:00.000Z",
            "ends_at": null,
            "every": 1,
            "starts_at": "2026-01-31T00:00:00.000Z",
            "unit": "month"
        }
    });
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
    wallet.receive(None).await.unwrap();
    let request = wallet.requests().await.unwrap().remove(0).request;
    let period = PeriodRef {
        starts_at: 1_769_817_600_000,
        ends_at: 1_772_236_800_000,
    };
    let receipt = paykit_lib::bodies::PaymentReceipt {
        attempt_id: paykit_lib::AttemptId::from_bytes([3; 16]),
        billing_period: Some(period),
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
    .unwrap();
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_receipt",
        vec![[7; 32], request.request_hash],
        receipt,
    );
    let keys = w.bob.identity().app_keys("shop");
    let cert = w.bob.switch().app(&keys.app_id_bytes()).cert.unwrap();
    let stored = paykit_lib::records::StoredMessage::new(
        w.bob_id(),
        cert.signed(),
        plaintext,
        Some(w.clock.now_ms()),
        &w.alice_id(),
    )
    .unwrap();
    let mut record = paykit_lib::records::ActivityRecord::new(
        paykit_lib::records::ActivitySide::Payer,
        w.bob_id(),
    );
    record.request_hash = Some(request.request_hash);
    record.billing_period = Some(period);
    record.receipt = Some(stored);
    let collection = paykit_lib::collections::Collection::PayerActivity;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::period_record(
            &w.bob_id(),
            &request.request_hash,
            period.starts_at / 1000,
        ),
    );
    wallet
        .app_mut()
        .put_record(&target, &record.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    let reference = PaymentReference::new("plan-7").unwrap();
    let found = wallet
        .receipts(ReceiptsOf::Reference(reference))
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].billing_period, Some(period));
    assert_eq!(found[0].payment_reference.as_deref(), Some("plan-7"));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiptClaimState {
    OtherAttempt,
    Settled,
    Expired,
    Absent,
}

impl ReceiptClaimState {
    async fn prepare(
        self,
        w: &World,
        payer: &mut paykit_sdk::Paykit,
        obligation: [u8; 32],
        attempt: paykit_lib::AttemptId,
        proof_hash: [u8; 32],
        deadline: u64,
    ) -> Option<pubky_switch_core::claims::ClaimView> {
        use pubky_switch_client::app::{ClaimOp, OpBatch};
        use pubky_switch_core::claims::{Basis, ClaimStatus, RECONCILIATION_MS};
        if self != Self::Absent {
            let id = if self == Self::OtherAttempt {
                [4; 16]
            } else {
                attempt.to_bytes()
            };
            let begin = ClaimOp::attempt(obligation, 0, id, 300, Some(deadline));
            payer
                .app_mut()
                .submit_batch(OpBatch::new().op(begin.kind()))
                .await
                .unwrap();
            if self != Self::OtherAttempt {
                let done =
                    ClaimOp::done(obligation, 1, id, Basis::Owner, Some(proof_hash)).unwrap();
                payer
                    .app_mut()
                    .submit_batch(OpBatch::new().op(done.kind()))
                    .await
                    .unwrap();
            }
        }
        if self == Self::Expired {
            w.clock.set_ms(deadline + RECONCILIATION_MS);
        }
        let before = payer
            .app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .map(|c| c.view);
        match self {
            Self::OtherAttempt => {
                let claim = before.as_ref().unwrap();
                assert_eq!(claim.status, ClaimStatus::Attempting);
                assert!(claim.attempt(&attempt.to_bytes()).is_none());
            }
            Self::Settled => {
                let claim = before.as_ref().unwrap();
                assert_eq!(claim.status, ClaimStatus::Settled);
                assert!(claim.tombstone);
            }
            Self::Expired | Self::Absent => assert!(before.is_none()),
        }

        before
    }
}

async fn requested_receipt_without_proof(state: ReceiptClaimState) {
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    use paykit_lib::AttemptId;

    let w = World::new();
    let storage = crate::common::store();
    let mut reader = w.wallet_on("reader", storage.clone()).await;
    let mut payer = w.wallet("payer").await;
    let mut shop = w.shop().await;
    let draft = draft("1", w.clock.now_ms());
    let deadline = draft.proposal_expires_at_ms.unwrap() + 1;
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    reader.receive(None).await.unwrap();
    payer.receive(None).await.unwrap();
    let attempt = AttemptId::from_bytes([3; 16]);
    let proof = requested_proof(&w, &payer, &shop, &request, attempt);
    let obligation = paykit_lib::derive::request_obligation(
        reader.app().naming_key(),
        &request.payee,
        &request.request_hash,
        0,
    );
    let before = state
        .prepare(&w, &mut payer, obligation, attempt, proof.hash(), deadline)
        .await;
    assert_eq!(
        reader
            .app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .map(|c| c.view),
        before
    );
    let receipt = signed_receipt(&w, &shop, &request, &proof);
    let app_id = reader.app_id();
    w.deliver(&w.bob, "shop", &w.alice, &[app_id], receipt.plaintext());
    reader.receive(None).await.unwrap();
    let kept = reader.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(kept.len(), 1, "{state:?}");
    assert!(kept[0].0.ends_with(receipt.message().msg_id.as_bytes()));
    let value: serde_json::Value = serde_json::from_slice(&kept[0].1).unwrap();
    assert_eq!(
        value["d"]["plaintext"],
        pubky_switch_core::enc::b64(&receipt.plaintext().encode())
    );
    assert_eq!(
        value["d"]["cert"],
        serde_json::to_value(receipt.cert().signed()).unwrap()
    );
    assert!(reader
        .receipts(ReceiptsOf::Request(request))
        .await
        .unwrap()
        .is_empty());
    drop(reader);
    let mut reader = w
        .load_on_clock(&w.alice, storage, app_id, w.clock.clone())
        .await;
    reader.receive(None).await.unwrap();
    assert_eq!(
        reader.app().protocol_scan("paykit", b"pr/").await.unwrap(),
        kept
    );

    if state == ReceiptClaimState::Absent {
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
        reader.receive(None).await.unwrap();
        assert!(reader
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .is_empty());
        let notices = reader.notices().await.unwrap();
        assert!(notices.iter().any(
            |notice| notice.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && notice.msg_id.as_ref() == Some(&receipt.message().msg_id)
        ));
        return;
    }
    let target = crate::outcome_record::payer_record(&request);
    publish_requested_proof(&mut payer, &request, proof).await;
    reader.receive(None).await.unwrap();
    let placed = reader.receipts(ReceiptsOf::Request(request)).await.unwrap();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].msg_id, receipt.message().msg_id);
    let held = reader
        .app()
        .shared_record(&reader.app().record_key(&target).unwrap())
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    assert_eq!(record.receipt.unwrap().hash(), receipt.hash());
    assert!(reader
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        reader
            .app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .map(|c| c.view),
        before
    );
}

#[tokio::test]
async fn test_requested_receipt_outlives_an_unmatched_attempt() {
    Box::pin(requested_receipt_without_proof(
        ReceiptClaimState::OtherAttempt,
    ))
    .await;
}

#[tokio::test]
async fn test_requested_receipt_outlives_a_settled_claim() {
    Box::pin(requested_receipt_without_proof(ReceiptClaimState::Settled)).await;
}

#[tokio::test]
async fn test_requested_receipt_outlives_an_expired_claim() {
    Box::pin(requested_receipt_without_proof(ReceiptClaimState::Expired)).await;
}

#[tokio::test]
async fn test_requested_receipt_without_a_claim_expires_from_its_original_acceptance() {
    Box::pin(requested_receipt_without_proof(ReceiptClaimState::Absent)).await;
}
