//! A single enrolled wallet may owe both roles of an early payment cancellation.

use super::*;
use paykit_lib::records::StoredMessage;
use pubky_dispatch_client::app::{
    AppEvent, Collection, DropReason, FeedPosition, OutgoingMessage, ParkReason, RecordTarget,
    SignedMessage, WaitingReason,
};
use serde_json::{json, Value};

#[tokio::test]
async fn test_dual_role_cancellation_reopened_at_the_bound_scripted() -> TestResult {
    cancellation_at_the_bound(Backend::Scripted, WaitingReply::Admitted).await
}

#[tokio::test]
async fn test_dual_role_cancellation_reopened_at_the_bound_postgres() -> TestResult {
    cancellation_at_the_bound(Backend::Service, WaitingReply::Admitted).await
}

#[tokio::test]
async fn test_dual_role_cancellation_restored_past_the_wait_scripted() -> TestResult {
    cancellation_at_the_bound(Backend::Scripted, WaitingReply::Expired).await
}

#[tokio::test]
async fn test_dual_role_cancellation_restored_past_the_wait_postgres() -> TestResult {
    cancellation_at_the_bound(Backend::Service, WaitingReply::Expired).await
}

#[tokio::test]
async fn test_waiting_reply_overflow_keeps_the_original_cancellation() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        cancellation_at_the_bound(backend, WaitingReply::Evicted).await?;
    }
    Ok(())
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum WaitingReply {
    Admitted,
    Expired,
    Evicted,
}

async fn cancellation_at_the_bound(backend: Backend, outcome: WaitingReply) -> TestResult {
    let Some(world) = World::start(backend)? else {
        return Ok(());
    };
    let result = Box::pin(dual_role_wallet(&world, outcome)).await;
    world.close().await?;
    result
}

fn original_request(sender: &Paykit, recipient: [u8; 32]) -> TestResult<SignedMessage> {
    let body = json!({"amount":{"asset":"btc","value":"1"},"payment_reference":"order",
        "proposal_expires_at":null,"payment_endpoints":{"btc-lightning-bolt11":"lnbc1invoice"}});
    Ok(sender.app().sign_message(OutgoingMessage::new(
        recipient,
        "paykit.payment_request",
        body.as_object().unwrap().clone(),
    ))?)
}

fn answer(
    sender: &Paykit,
    recipient: [u8; 32],
    kind: &str,
    original: &SignedMessage,
) -> TestResult<SignedMessage> {
    let body = json!({"reason":null, "label":null});
    let mut message = OutgoingMessage::new(recipient, kind, body.as_object().unwrap().clone());
    message.refs.push(original.hash());
    Ok(sender.app().sign_message(message)?)
}

fn target(family: paykit_lib::Collection, seller: &[u8; 32], hash: &[u8; 32]) -> RecordTarget {
    RecordTarget::new(
        Collection::new(family.name(), family.policy(None)),
        &natural_key::request_record(seller, hash),
    )
}

async fn read_duties(wallet: &Paykit, prefix: &[u8]) -> TestResult<Vec<Value>> {
    let mut rows = Vec::new();
    for (_, bytes) in wallet.app().protocol_scan("paykit", prefix).await? {
        let stored: Value = serde_json::from_slice(&bytes)?;
        rows.push(stored["d"].clone());
    }
    Ok(rows)
}

async fn follow_wallet(
    world: &World,
    signer: &mut Signer,
    wallet: &mut Paykit,
    peer: &mut Paykit,
    old: &str,
    next: &str,
) -> TestResult {
    let owner = world.identity("customer")?;
    world.lose_switch(old);
    world.add_switch(next, &[&owner]).await?;
    Box::pin(signer.declare_provider_loss(
        next,
        pubky_dispatch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    Box::pin(signer.end_staged_admission()).await?;
    wallet.app_mut().refresh_identity().await?;
    wallet.app_mut().follow_provider().await?;
    while wallet.app_mut().list_collections().await? {}
    wallet.receive(None).await?;
    wallet.app_mut().invite(&peer.identity()).await?;
    peer.app_mut().identity_of(&wallet.identity(), true).await?;
    Ok(())
}

async fn dual_role_wallet(world: &World, outcome: WaitingReply) -> TestResult {
    let first = "https://first.customer.test";
    let second = "https://second.customer.test";
    let third = "https://third.customer.test";
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let store = Arc::new(MemoryStore::new());
    let mut customer = Box::pin(setup_identity(world, "customer", first)).await?;
    let mut merchant = Box::pin(setup_identity(
        world,
        "merchant",
        "https://merchant.payments.test",
    ))
    .await?;
    let scopes = AppScopes::new().pay().request().build()?;
    let mut wallet = Box::pin(join(
        world,
        &mut customer,
        "customer",
        "wallet",
        scopes.clone(),
        store.clone(),
        &rail,
    ))
    .await?;
    let mut peer = Box::pin(join(
        world,
        &mut merchant,
        "merchant",
        "counter",
        scopes,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    let own = wallet.identity();
    let other = peer.identity();
    wallet.app_mut().invite(&other).await?;
    peer.app_mut().invite(&own).await?;

    // Two authentic replies share one service millisecond on opposite sides of a loss.
    // After another loss the two positions can no longer be ordered.
    let purchase = original_request(&peer, own)?;
    let payer_target = target(
        paykit_lib::Collection::PayerActivity,
        &other,
        &purchase.hash(),
    );
    // The old service saw several versions of the unreadable payer record. Recovery
    // uploads its latest value once, so the replacement has the smaller inbox sequence.
    for version in 0..12 {
        let value = serde_json::to_vec(&json!({"unrecognized_activity": version}))?;
        wallet
            .app_mut()
            .put_record(&payer_target, &value, Vec::new())
            .await?;
    }
    let sale = original_request(&wallet, other)?;
    let mut positions = Vec::new();
    let mut times = Vec::new();
    let other_sale = original_request(&wallet, other)?;
    for (old, next, original) in [(first, second, &sale), (second, third, &other_sale)] {
        let reply = answer(&peer, own, "paykit.payment_request_rejection", original)?;
        let plain = pubky_dispatch_core::enc::b64(&reply.plaintext().encode());
        let sent = peer.app_mut().queue_signed(reply).await?;
        peer.flush().await?;
        let delivery = peer.app().delivery(&sent).await?;
        assert!(
            matches!(
                delivery,
                Some(pubky_dispatch_client::app::Delivery::Accepted { .. })
            ),
            "{old}: {delivery:?}"
        );
        wallet.receive(None).await?;
        let received = read_duties(&wallet, b"pr/")
            .await?
            .into_iter()
            .find(|r| r["plaintext"] == plain)
            .ok_or("the delivered payer reply was not retained")?;
        times.push(
            received["accepted_at"]
                .as_u64()
                .ok_or("missing acceptance time")?,
        );
        positions.push(FeedPosition {
            generation: received["generation"]
                .as_u64()
                .ok_or("missing generation")?,
            seq: received["seq"].as_u64().ok_or("missing sequence")?,
        });
        Box::pin(follow_wallet(
            world,
            &mut customer,
            &mut wallet,
            &mut peer,
            old,
            next,
        ))
        .await?;
    }
    assert_eq!(times[0], times[1]);
    assert_ne!(positions[0].generation, positions[1].generation);
    assert_eq!(
        wallet.app().received_order(positions[0], positions[1]),
        None,
        "{positions:?}"
    );

    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    fill_replies(world, &mut wallet, &mut peer, bound - 2).await?;
    assert_eq!(read_duties(&wallet, b"pr/").await?.len(), bound);

    let stop = answer(&peer, own, "paykit.payment_request_cancellation", &purchase)?;
    peer.app_mut().queue_signed(stop.clone()).await?;
    peer.flush().await?;
    wallet.receive(None).await?;
    let parked = wallet.app_mut().parked_entries().await?;
    let waiting = parked
        .iter()
        .find(|entry| entry.reason == ParkReason::AwaitingAdmission)
        .ok_or("no original waiting for reply admission")?;
    let original = wallet
        .app_mut()
        .parked_entry(waiting.stored_id)
        .await?
        .ok_or("missing original")?;
    assert!(original.sender_verified);
    assert_eq!(&original.plaintext, stop.plaintext());
    let saved = read_duties(&wallet, b"kc/").await?;
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0]["plaintext"],
        pubky_dispatch_core::enc::b64(&stop.plaintext().encode())
    );
    assert_eq!(saved[0]["cert"], json!(original.cert));
    assert_eq!(saved[0]["accepted_at"], original.accepted_at_ms);
    assert_eq!(saved[0]["generation"], original.generation);
    assert_eq!(saved[0]["seq"], original.seq);
    assert_eq!(read_duties(&wallet, b"pr/").await?.len(), bound);
    let mut wallet = if outcome == WaitingReply::Expired {
        let backup = wallet.backup().await?;
        drop(wallet);
        Box::pin(Paykit::restore(
            world.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            &backup,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?
    } else {
        let id = wallet.app_id();
        drop(wallet);
        Box::pin(Paykit::load(
            world.app_env(store),
            Default::default(),
            id,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?
    };
    for _ in 0..4 {
        wallet.receive(None).await?;
    }
    let after = wallet
        .app_mut()
        .parked_entry(original.stored_id)
        .await?
        .ok_or("missing original after restart")?;
    assert_eq!(after.plaintext, original.plaintext);
    assert_eq!(
        (after.accepted_at_ms, after.generation, after.seq),
        (original.accepted_at_ms, original.generation, original.seq)
    );
    assert_eq!(read_duties(&wallet, b"kc/").await?, saved);
    match outcome {
        WaitingReply::Expired => {
            world.advance_ms(22 * pubky_dispatch_core::records::DAY_MS);
            wallet.receive(None).await?;
        }
        WaitingReply::Admitted => {
            Box::pin(place_sale(&mut wallet, &sale, other)).await?;
            for _ in 0..4 {
                wallet.receive(None).await?;
            }
        }
        WaitingReply::Evicted => {
            fill_replies(world, &mut wallet, &mut peer, bound).await?;
            let notices = wallet.app_mut().unacknowledged_events().await?;
            let matching = notices.iter().filter(|event| matches!(event,
                AppEvent::StoredEntryDropped { sender, seq, generation, message, accepted_at_ms,
                    sender_verified: true, waiting: WaitingReason::Admission, reason: DropReason::PerSender, .. }
                if sender == &other && message.msg_id == stop.msg_id() && message.hash == stop.hash()
                    && *seq == original.seq && *generation == original.generation
                    && *accepted_at_ms == original.accepted_at_ms));
            assert_eq!(matching.count(), 1);
        }
    }
    let replies = read_duties(&wallet, b"pr/").await?;
    assert_eq!(replies.len(), bound);
    assert_eq!(
        replies.iter().filter(|r| *r == &saved[0]).count(),
        usize::from(outcome == WaitingReply::Admitted)
    );
    let parked = wallet.app_mut().parked_entries().await?;
    assert!(!parked.iter().any(|p| p.stored_id == original.stored_id));
    let waiting = parked
        .iter()
        .filter(|p| p.reason == ParkReason::AwaitingAdmission)
        .count();
    assert_eq!(
        waiting,
        if outcome == WaitingReply::Evicted {
            bound
        } else {
            0
        }
    );
    assert_eq!(read_duties(&wallet, b"kc/").await?, saved);

    // The payer record's readable value finally arrives, holding the authentic purchase:
    // the kept stop is applied to it.
    Box::pin(place_purchase(&mut wallet, &peer, &purchase, &payer_target)).await?;
    peer.app_mut().queue_signed(purchase.clone()).await?;
    peer.flush().await?;
    for _ in 0..3 {
        wallet.receive(None).await?;
    }
    let row = wallet
        .app()
        .shared_record(&wallet.app().record_key(&payer_target)?)
        .await?
        .ok_or("missing purchase")?;
    let current = ActivityRecord::read(
        row.content.value().ok_or("missing signed request")?,
        &own,
        ActivitySide::Payer,
    )?;
    assert_eq!(
        current.request.ok_or("missing original")?.hash(),
        purchase.hash()
    );
    assert_eq!(
        current
            .cancellation
            .ok_or("missing applicable stop")?
            .plaintext(),
        stop.plaintext()
    );
    assert!(read_duties(&wallet, b"kc/").await?.is_empty());
    assert!(rail.all_executions().is_empty());
    Ok(())
}

/// The payer record at `payer_target` written readable, holding the peer's signed `purchase`.
async fn place_purchase(
    wallet: &mut Paykit,
    peer: &Paykit,
    purchase: &SignedMessage,
    payer_target: &RecordTarget,
) -> TestResult {
    let own = wallet.identity();
    let other = peer.identity();
    let signed = StoredMessage::new(
        other,
        peer.app().cert().signed(),
        purchase.plaintext().clone(),
        None,
        &own,
    )?;
    let mut record = ActivityRecord::new(ActivitySide::Payer, other);
    record.msg_ids.push(purchase.msg_id().to_owned());
    record.request = Some(signed);
    wallet
        .app_mut()
        .put_record(payer_target, &record.to_json()?, Vec::new())
        .await?;
    Ok(())
}

/// The authentic original of one old seller-side reply finally arrives in shared activity.
async fn place_sale(wallet: &mut Paykit, sale: &SignedMessage, other: [u8; 32]) -> TestResult {
    let own = wallet.identity();
    let signed = StoredMessage::new(
        own,
        wallet.app().cert().signed(),
        sale.plaintext().clone(),
        None,
        &other,
    )?;
    let mut record = ActivityRecord::new(ActivitySide::Payee, other);
    record.msg_ids.push(sale.msg_id().to_owned());
    record.request = Some(signed);
    wallet
        .app_mut()
        .put_record(
            &target(paykit_lib::Collection::PayeeActivity, &own, &sale.hash()),
            &record.to_json()?,
            Vec::new(),
        )
        .await?;
    Ok(())
}

async fn fill_replies(
    world: &World,
    wallet: &mut Paykit,
    peer: &mut Paykit,
    count: usize,
) -> TestResult {
    for _ in 0..count {
        world.advance_ms(60_001);
        let other_sale = original_request(wallet, peer.identity())?;
        let rejection = answer(
            peer,
            wallet.identity(),
            "paykit.payment_request_rejection",
            &other_sale,
        )?;
        let sent = peer.app_mut().queue_signed(rejection).await?;
        // A rejection goes only on a fresh copy of the deny list (Paykit spec 8.1).
        peer.flush().await?;
        let delivery = peer.app_mut().flush_message(&sent).await?;
        assert!(
            matches!(
                delivery,
                Some(pubky_dispatch_client::app::Delivery::Accepted { .. })
            ),
            "{delivery:?}"
        );
    }
    for _ in 0..=count {
        if !wallet.app_mut().receive(None).await?.more {
            break;
        }
    }
    Ok(())
}
