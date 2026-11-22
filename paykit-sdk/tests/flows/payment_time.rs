//! The payee credits a one-time request only on evidence whose verified payment time lies
//! between the request's signed `created_at`, less the method's declared tolerance, and the
//! request's deadline (Paykit spec 7.1; 7.2, "Not before the request").

use paykit_sdk::rail::{Behaviour, FAKE_LONGEST_COMPLETION_MS};
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, RailDeclaration, Refusal, RequestDraft};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, World};

/// A one-time request created now, `deadline_after_ms` giving it a payment deadline that long
/// after its creation, is paid through a payment left in flight, and the method then settles
/// it with evidence dated `offset_ms` from the request's creation, the payee's method
/// declaring `tolerance_ms`. Returns the time the payee credited, or `None` when it flagged the
/// evidence as outside the request's times, with the evidence's time and the payee's status
/// after it tried to confirm the payment.
async fn credit_dated(
    tolerance_ms: u64,
    deadline_after_ms: Option<u64>,
    offset_ms: i64,
) -> (Option<u64>, u64, PayeeStatus) {
    let w = World::new();
    w.rail.set_declaration(
        RailDeclaration::new(FAKE_LONGEST_COMPLETION_MS, tolerance_ms)
            .with_history(true)
            .with_history_survives_restore(true),
    );
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let created = w.clock.now_ms();
    let mut terms: RequestDraft = draft("1", created);
    if let Some(after) = deadline_after_ms {
        terms = terms.with_payment_deadline_ms(created + after);
    }
    let request = request_from(&mut shop, w.alice_id(), terms).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let pending = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    let paid_at = created.checked_add_signed(offset_ms).unwrap();
    w.rail.settle_at(&pending.attempt_id(), paid_at);
    w.clock.advance_ms(1_000);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let flagged = |result: &paykit_sdk::PaykitError| {
        assert_eq!(
            result.refusal(),
            Some(&Refusal::PaymentTimeOutside {
                paid_at_ms: paid_at
            }),
            "{result:?}"
        );
    };
    let verified = match shop.verify_payment(&record).await {
        Ok(credited) => {
            assert_eq!(credited, Some(paid_at));
            assert!(shop.confirm_payment(&record).await.unwrap().is_some());
            credited
        }
        Err(error) => {
            flagged(&error);
            flagged(&shop.confirm_payment(&record).await.unwrap_err());
            None
        }
    };
    let status = shop.payee_activity().await.unwrap()[0].status.clone();
    (verified, paid_at, status)
}

#[tokio::test]
async fn test_payment_time_evidence_at_the_tolerance_is_credited() {
    let (verified, paid_at, status) = credit_dated(1_000, None, -1_000).await;
    assert_eq!(verified, Some(paid_at));
    assert_eq!(status, PayeeStatus::Receipted);
}

#[tokio::test]
async fn test_payment_time_evidence_past_the_tolerance_is_not_credited() {
    let (verified, _, status) = credit_dated(1_000, None, -1_001).await;
    assert_eq!(verified, None);
    assert_eq!(status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_payment_time_funds_settled_before_a_later_request_are_not_credited() {
    // Funds that reached a reusable endpoint before the request existed don't prove it.
    let (verified, _, status) = credit_dated(0, None, -1).await;
    assert_eq!(verified, None);
    assert_eq!(status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_payment_time_evidence_at_the_deadline_is_credited() {
    let (verified, paid_at, status) = credit_dated(0, Some(60_000), 60_000).await;
    assert_eq!(verified, Some(paid_at));
    assert_eq!(status, PayeeStatus::Receipted);
}

#[tokio::test]
async fn test_payment_time_evidence_past_the_deadline_is_not_credited() {
    let (verified, _, status) = credit_dated(0, Some(60_000), 60_001).await;
    assert_eq!(verified, None);
    assert_eq!(status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_payment_time_tolerance_does_not_extend_the_deadline() {
    let (verified, _, status) = credit_dated(10_000, Some(60_000), 60_001).await;
    assert_eq!(verified, None);
    assert_eq!(status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_payment_time_evidence_after_the_proposal_expiry_is_not_credited() {
    // The test request's proposal expires a day after its creation, its only deadline.
    let day = 24 * crate::common::HOUR_MS;
    let (verified, _, status) = credit_dated(0, None, i64::try_from(day + 1).unwrap()).await;
    assert_eq!(verified, None);
    assert_eq!(status, PayeeStatus::ProofReceived);
}
