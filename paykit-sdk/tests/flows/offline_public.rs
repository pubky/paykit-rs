use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, PaykitConfig, PaymentMode, Stage,
};
use pubky_dispatch_client::app::{AppConfig, UserConfirmed};
use pubky_dispatch_core::claims::Outcome;

use crate::common::{btc, endpoint, store, World};

mod recording;

#[tokio::test]
async fn test_offline_public_payment_is_recorded_once_on_reconnect() {
    pay_public(true).await;
}

#[tokio::test]
async fn test_offline_public_same_selection_pays_once_while_connected() {
    pay_public(false).await;
}

#[tokio::test]
async fn test_offline_public_refused_call_is_no_payment_started() {
    // The method refuses an offline payment's call: nothing was submitted. Like a refused
    // online call it is kept as no payment started, never as the method's failure, whose
    // history the recheck would ask about for nothing.
    let world = World::new();
    let mut wallet = world
        .wallet_with("wallet", store(), PaykitConfig::default())
        .await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    world.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let outcome = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Failed {
        attempt_id, reason, ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert!(
        reason.starts_with("the payment method refused the call"),
        "{reason}"
    );
    let key = format!("j/{}", attempt_id.to_text());
    let entry: serde_json::Value = serde_json::from_slice(
        &wallet
            .app()
            .protocol_get("paykit", key.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        entry["d"]["failed_before_call"],
        serde_json::json!(true),
        "{entry}"
    );
    assert_eq!(
        entry["d"]["failed_by_method"],
        serde_json::json!(false),
        "{entry}"
    );
    let facts = format!("jf/{}", attempt_id.to_text());
    assert!(wallet
        .app()
        .protocol_get("paykit", facts.as_bytes())
        .await
        .unwrap()
        .is_none());
    assert!(world.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_offline_public_payment_rechecks_the_selected_transfer_before_handoff() {
    let world = World::new();
    let mut wallet = world
        .wallet_with("wallet", store(), PaykitConfig::default())
        .await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    // Planned at the amount paid, the method would transfer another just before handoff.
    world
        .rail
        .next_transfer_amounts(vec![Some(btc("0.001")), Some(btc("0.002"))]);

    let error = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap_err();
    let Some(paykit_sdk::Refusal::Transfer { quote, .. }) = error.refusal() else {
        panic!("{error:?}");
    };
    assert_eq!(quote.transfer.amount, Some(btc("0.002")));
    assert!(world.rail.all_executions().is_empty());
}

pub(crate) async fn shop_with_endpoint(world: &World) -> paykit_sdk::Paykit {
    let mut shop = world
        .join_with(
            &world.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_app(AppConfig {
                publishes_record: true,
                ..AppConfig::default()
            }),
            store(),
        )
        .await;
    let chosen = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(
        chosen.clone(),
        PaymentEndpointPayload::new("lno1tips").unwrap(),
    )
    .await
    .unwrap();
    shop.app_mut().invite(&world.alice_id()).await.unwrap();
    shop
}

pub(crate) fn payment(world: &World) -> paykit_sdk::UnrequestedPayment {
    paykit_sdk::UnrequestedPayment::new(world.bob_id(), btc("0.001")).with_options(
        PayOptions::default()
            .with_endpoint(endpoint("btc-lightning-bolt12"))
            .with_mode(PaymentMode::Public(UserConfirmed::new())),
    )
}

async fn pay_public(disconnect: bool) {
    let world = World::new();
    let mut wallet = world.wallet("wallet").await;
    let chosen = endpoint("btc-lightning-bolt12");
    let mut shop = shop_with_endpoint(&world).await;
    let offer =
        pubky_dispatch_core::records::AppRecord::verify(shop.app().record(), &world.bob_id())
            .unwrap();
    assert_eq!(offer.expires_at(), None);
    wallet.receive(None).await.unwrap();
    if disconnect {
        world.router.remove(world.alice.origin());
    }
    let outcome = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap();
    let attempt_id = match (disconnect, outcome) {
        (true, PayOutcome::PaidOutcomeUnwritten { attempt_id, .. })
        | (false, PayOutcome::Paid { attempt_id, .. }) => attempt_id,
        (_, other) => panic!("{other:?}"),
    };
    assert_eq!(world.rail.executions(&attempt_id), 1);
    let entries = wallet.payments().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].endpoint_id, chosen);
    assert_eq!(entries[0].attempt_id, attempt_id);
    assert_eq!(world.rail.paid_amount(&attempt_id), Some(btc("0.001")));
    if disconnect {
        assert_eq!(entries[0].stage, Stage::RailPaid);
        shop.receive(None).await.unwrap();
        assert!(shop.payee_activity().await.unwrap().is_empty());
        world.clock.advance_ms(30 * 60_000);
        world
            .router
            .add_switch(world.alice.origin(), world.alice.switch());
    }
    wallet.resume_payments().await.unwrap();
    wallet.resume_payments().await.unwrap();
    let claim = wallet
        .app_mut()
        .read_claim(&entries[0].obligation)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(claim.attempts.len(), 1);
    assert_eq!(
        claim.attempt(&attempt_id.to_bytes()).unwrap().outcome,
        Outcome::Done
    );
    let recorded = wallet.payments().await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].attempt_id, attempt_id);
    assert_eq!(recorded[0].stage, Stage::Finished);
    assert_eq!(world.rail.executions(&attempt_id), 1);
    assert_eq!(world.rail.pay_calls(&attempt_id), 1);
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_offline_public_paid_journal_after_restore_waits_on_silent_claim() {
    use paykit_sdk::{Paykit, Rail};
    use std::sync::Arc;
    let world = World::new();
    let mut wallet = world.wallet("wallet").await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    let id = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    world
        .router
        .add_switch(world.alice.origin(), world.alice.switch());
    let mut restored = Paykit::restore(
        world.env(&world.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&world.rail) as Arc<dyn Rail>,
        world.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    restored.resume_payments().await.unwrap();
    assert!(restored
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    assert_eq!(restored.payments().await.unwrap()[0].stage, Stage::RailPaid);
    assert_eq!(world.rail.pay_calls(&id), 1);
}

#[tokio::test]
async fn test_offline_public_late_success_records_original_attempt() {
    use paykit_sdk::rail::Behaviour;
    let world = World::new();
    let mut wallet = world.wallet("wallet").await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    world.rail.next(Behaviour::Fail);
    let outcome = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Failed { .. }));
    let id = outcome.attempt_id();
    world
        .router
        .add_switch(world.alice.origin(), world.alice.switch());
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    // The claim the payment would have written through the dispatch: its attempt ID blinded
    // with the payer's naming key (Paykit wire 1.5).
    assert_eq!(
        obligation,
        paykit_lib::derive::payment_obligation(wallet.app().naming_key(), &id)
    );
    assert!(wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    world.rail.complete_after_failure(&id);
    wallet.resume_payments().await.unwrap();
    wallet.resume_payments().await.unwrap();
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(claim.attempts.len(), 1);
    assert_eq!(
        claim.attempt(&id.to_bytes()).unwrap().outcome,
        Outcome::Done
    );
    assert_eq!(world.rail.pay_calls(&id), 1);
}

#[tokio::test]
async fn test_offline_public_existing_claim_keeps_its_original_deadline() {
    use pubky_dispatch_client::app::{ClaimOp, OpBatch};
    let world = World::new();
    let mut wallet = world.wallet("wallet").await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    let id = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let key = wallet.payments().await.unwrap()[0].obligation;
    world
        .router
        .add_switch(world.alice.origin(), world.alice.switch());
    let op = ClaimOp::attempt(key, 0, id.to_bytes(), 300, None);
    wallet
        .app_mut()
        .submit_batch(OpBatch::new().op(op.kind()))
        .await
        .unwrap();
    wallet.resume_payments().await.unwrap();
    wallet.resume_payments().await.unwrap();
    let claim = wallet
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(claim.deadline, None);
    assert_eq!(claim.attempts.len(), 1);
    assert_eq!(
        claim.attempt(&id.to_bytes()).unwrap().outcome,
        Outcome::Done
    );
    assert_eq!(world.rail.pay_calls(&id), 1);
}

#[tokio::test]
async fn test_offline_public_lost_provider_does_not_release_an_unanswered_recording() {
    use pubky_dispatch_client::app::ClaimAttempt;
    use pubky_dispatch_client::testing::Fault;
    let world = World::new();
    let mut wallet = world.wallet("wallet").await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    let id = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let key = wallet.payments().await.unwrap()[0].obligation;
    world
        .router
        .add_switch(world.alice.origin(), world.alice.switch());
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    // A second unanswered send also covers the ordinary outbox pass at the end of resume.
    world.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    wallet.resume_payments().await.unwrap();
    let kept = wallet
        .app()
        .kept_attempt(ClaimAttempt {
            claim_key: key,
            attempt_id: id.to_bytes(),
        })
        .await
        .unwrap();
    assert!(kept.is_some());
    crate::provider_loss::declare_loss(&world);
    crate::provider_loss::follow_loss(&mut wallet).await;
    wallet.resume_payments().await.unwrap();
    assert!(wallet.app_mut().read_claim(&key).await.unwrap().is_none());
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::RailPaid);
    assert_eq!(world.rail.pay_calls(&id), 1);
}

#[tokio::test]
async fn test_offline_public_applied_recording_with_lost_answer_finishes_after_expiry() {
    use pubky_dispatch_client::testing::Fault;
    let world = World::new();
    let mut wallet = world.wallet("wallet").await;
    let _shop = shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    let id = wallet
        .pay_without_request(payment(&world))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let key = wallet.payments().await.unwrap()[0].obligation;
    world
        .router
        .add_switch(world.alice.origin(), world.alice.switch());
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    // The final ordinary flush also misses the answer. The service has already applied
    // the original attempt and payer record in one transaction.
    world.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    wallet.resume_payments().await.unwrap();
    let written = wallet
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(
        written.attempt(&id.to_bytes()).unwrap().outcome,
        Outcome::Open
    );
    world.clock.advance_ms(600_001);
    wallet.resume_payments().await.unwrap();
    let view = wallet
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(view.attempts.len(), 1);
    assert_eq!(view.deadline, written.deadline);
    assert_eq!(view.attempt(&id.to_bytes()).unwrap().outcome, Outcome::Done);
    assert_eq!(world.rail.pay_calls(&id), 1);
}
