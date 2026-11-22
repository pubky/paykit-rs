//! The payee credits a one-time request only on evidence whose verified payment time lies
//! between the request's signed `created_at`, less the method's declared tolerance, and the
//! request's payment deadline (Paykit spec 7.1, including "Not before the request").

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
    let (w, mut shop, record, paid_at) =
        paid_dated(tolerance_ms, deadline_after_ms, offset_ms).await;
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
    // Another payee app of the shop's identity reads the same record and decides alike.
    let mut second = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    second.receive(None).await.unwrap();
    let record = second.payee_activity().await.unwrap()[0].record;
    match second.verify_payment(&record).await {
        Ok(credited) => assert_eq!(credited, verified),
        Err(error) => {
            assert_eq!(verified, None);
            flagged(&error);
        }
    }
    (verified, paid_at, status)
}

/// The world, the shop and its record of a one-time request paid as [`credit_dated`]
/// describes, with the evidence's time, before the shop verifies it.
async fn paid_dated(
    tolerance_ms: u64,
    deadline_after_ms: Option<u64>,
    offset_ms: i64,
) -> (World, paykit_sdk::Paykit, [u8; 32], u64) {
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
    (w, shop, record, paid_at)
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
async fn test_payment_time_settled_after_the_proposal_expiry_is_credited() {
    // The proposal expiry, a day after the request's creation here, bounds when the attempt
    // may start, not when the payment settles: one started inside it and settled after it
    // is credited (Paykit wire 4.5).
    let day = 24 * crate::common::HOUR_MS;
    let (verified, paid_at, status) = credit_dated(0, None, i64::try_from(day + 1).unwrap()).await;
    assert_eq!(verified, Some(paid_at));
    assert_eq!(status, PayeeStatus::Receipted);
}

#[tokio::test]
async fn test_payment_time_receipted_record_stays_credited_under_a_narrower_tolerance() {
    // Credited and receipted at the tolerance, the record stays credited when the payee's
    // method later declares none: the credit stands (Paykit spec 8.1 step 4).
    let (w, mut shop, record, paid_at) = paid_dated(1_000, None, -1_000).await;
    assert_eq!(shop.verify_payment(&record).await.unwrap(), Some(paid_at));
    let receipt = shop.confirm_payment(&record).await.unwrap();
    assert!(receipt.is_some());
    w.rail.set_declaration(
        RailDeclaration::new(FAKE_LONGEST_COMPLETION_MS, 0)
            .with_history(true)
            .with_history_survives_restore(true),
    );
    assert_eq!(shop.verify_payment(&record).await.unwrap(), Some(paid_at));
    assert_eq!(shop.confirm_payment(&record).await.unwrap(), receipt);
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Receipted
    );
}

#[tokio::test]
async fn test_payment_time_billing_period_evidence_before_the_request_is_not_credited() {
    // A Billing Period is bounded below by its request's signed `created_at` too.
    let w = World::new();
    w.rail.set_declaration(
        RailDeclaration::new(FAKE_LONGEST_COMPLETION_MS, 0)
            .with_history(true)
            .with_history_survives_restore(true),
    );
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let created = w.clock.now_ms();
    let sent = shop
        .send_request(w.alice_id(), crate::periods::monthly(created, None))
        .await
        .unwrap();
    let request = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    shop.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let pending = wallet
        .pay_period(&request, &crate::periods::january(), PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    let paid_at = created - 1;
    w.rail.settle_at(&pending.attempt_id(), paid_at);
    w.clock.advance_ms(1_000);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let refused = shop.verify_payment(&record).await.unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&Refusal::PaymentTimeOutside {
            paid_at_ms: paid_at
        }),
        "{refused:?}"
    );
}
