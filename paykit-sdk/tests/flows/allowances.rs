//! Allowances (Paykit spec 8.3): proposing, answering and ending, the `allowances` record on
//! either side, the kept end as the stop, the consent list and `executor_notice`.

use paykit_lib::allowances::AllowanceTerms;
use paykit_lib::grant::ExecutorGrant;
use paykit_lib::records::{AllowanceRecord, GrantEntry, PendingAllowanceRecord, StoredMessage};
use paykit_sdk::{
    AllowanceRole, AllowanceStatus, AppScopes, Paykit, PaykitError, Refusal, RequestedScopes,
    UserConfirmed,
};
use pubky_switch_client::app::{AppEvent, LocalWrite, OpBatch, OutgoingMessage};
use pubky_switch_client::clock::Clock;
use serde_json::{json, Value};

use crate::certificate::{is_not_now, list, renew, store_bundle};
use crate::common::{list_version, World, HOUR_MS};

fn terms() -> AllowanceTerms {
    AllowanceTerms::from_json(&json!({
        "asset": "btc",
        "per_payment_amount": { "minimum": "0.0001", "maximum": "0.01" },
        "period_limits": [],
        "lifetime_amount_limit": "0.10",
        "active_from": null,
        "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))
    .unwrap()
}

/// An app of alice's, `name`, with `scopes`; she admits bob.
async fn alice_app(w: &World, name: &str, scopes: RequestedScopes) -> Paykit {
    let mut app = w.join(&w.alice, name, scopes).await;
    app.app_mut().invite(&w.bob_id()).await.unwrap();
    app
}

/// Alice's wallet, holding `paykit_pay` and `paykit_allowance`.
async fn wallet(w: &World) -> Paykit {
    Box::pin(alice_app(
        w,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await
}

async fn paying_wallet(w: &World) -> Paykit {
    Box::pin(alice_app(
        w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await
}

/// Bob's shop, holding `paykit_request` and `paykit_allowance`; he admits alice.
async fn shop(w: &World) -> Paykit {
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop
}

fn refusal(error: &PaykitError) -> Option<&Refusal> {
    error.refusal()
}

/// Whether `summary` shows a message of `kind` received.
fn received(summary: &paykit_sdk::ReceiveSummary, kind: &str) -> bool {
    summary
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived { kind: k, .. } if k == kind))
}

/// The value of the record at `record` as `app` holds it.
async fn value(app: &Paykit, record: &[u8; 32]) -> Value {
    let held = app.app().shared_record(record).await.unwrap().unwrap();
    serde_json::from_slice(held.content.value().unwrap()).unwrap()
}

/// The service target for a pending proposal (Paykit wire 5.4).
fn pending_target(
    peer: &[u8; 32],
    id: &paykit_lib::AllowanceId,
) -> pubky_switch_client::app::RecordTarget {
    let family = paykit_lib::Collection::AllowanceProposals;
    pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(peer, id),
    )
}

/// The physical pending key behind the public Allowance key.
fn pending_key(app: &Paykit, peer: &[u8; 32], id: &paykit_lib::AllowanceId) -> [u8; 32] {
    app.app().record_key(&pending_target(peer, id)).unwrap()
}

async fn pending_at(app: &Paykit, public: &[u8; 32]) -> PendingAllowanceRecord {
    let view = app.allowance(public).await.unwrap();
    let key = pending_key(app, &view.counterparty, &view.allowance_id);
    let held = app.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        held.authenticated && !held.flagged && !held.not_taken && !held.divergent && !held.doubtful
    );
    PendingAllowanceRecord::read(held.content.value().unwrap(), &app.identity()).unwrap()
}

/// An external app's signed P/Q record, observed without giving this wallet a grant or spending authority.
async fn install_accepted_record(
    app: &mut Paykit,
    public: &[u8; 32],
    current: PendingAllowanceRecord,
    response: StoredMessage,
) {
    use pubky_switch_core::ops::{Condition, RecordVersion};
    let peer = *current.counterparty();
    let id = current.allowance_id().clone();
    let permanent = allowances_target(&peer, &id);
    assert_eq!(app.app().record_key(&permanent).unwrap(), *public);
    let accepted = AllowanceRecord {
        counterparty: peer,
        allowance_id: id.clone(),
        proposal: current.proposal().clone(),
        response: Some(response),
        end: None,
        conflicted: None,
        grant: None,
        grants: Vec::new(),
        msg_ids: Vec::new(),
        ledger_handoff: None,
        ledger_handoff_unread: None,
        unknown: Default::default(),
    };
    let value = accepted.to_json().unwrap();
    AllowanceRecord::read(&value, &app.identity()).unwrap();
    let create = app
        .app_mut()
        .record_op(
            &permanent,
            Some(&value),
            vec![Condition::RecordVersion {
                key: *public,
                version: RecordVersion::NONE,
            }],
        )
        .await
        .unwrap();
    let pending = pending_target(&peer, &id);
    let expected = app
        .app()
        .record_condition(&app.app().record_key(&pending).unwrap())
        .await
        .unwrap();
    let delete = app
        .app_mut()
        .record_op(&pending, None, vec![expected])
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(create).entry(delete))
        .await
        .unwrap();
}

/// The wallet that owns the budget confirms it before signing the original proposal.
async fn propose_in_role(
    app: &mut Paykit,
    peer: [u8; 32],
    role: AllowanceRole,
) -> paykit_sdk::Result<paykit_sdk::SentAllowance> {
    match role {
        AllowanceRole::Allower => {
            app.propose_executable_allowance(peer, terms(), UserConfirmed::new())
                .await
        }
        _ => app.propose_allowance(peer, role, terms()).await,
    }
}

/// Bob proposes an Allowance to alice as allowee; her wallet takes it. Returns the record
/// keys on bob's side and on alice's.
async fn proposed(w: &World, shop: &mut Paykit, wallet: &mut Paykit) -> ([u8; 32], [u8; 32]) {
    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let views = wallet.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
    (sent.record, views[0].record)
}

/// Sign an acceptance of the proposal at `record` and put it in the record for tests
/// that need an existing response. This helper returns the acceptance without queuing it.
/// This fixture models an original response observed at another app (Paykit spec 8.3.1).
async fn own_acceptance_recorded(app: &mut Paykit, record: &[u8; 32]) -> StoredMessage {
    let own = app.identity();
    let value = Box::pin(pending_at(app, record)).await;
    let peer = *value.counterparty();
    let mut message = OutgoingMessage::new(
        peer,
        "paykit.allowance_acceptance",
        json!({ "allowance_id": value.allowance_id().as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    message.refs = vec![value.proposal().hash()];
    let signed = app.app().sign_message(message).unwrap();
    let acceptance = StoredMessage::new(
        own,
        app.app().cert().signed(),
        signed.plaintext().clone(),
        None,
        &peer,
    )
    .unwrap();
    Box::pin(install_accepted_record(
        app,
        record,
        value,
        acceptance.clone(),
    ))
    .await;
    acceptance
}

/// The test identity whose identity key is `pubky`.
fn signer_of<'a>(w: &'a World, pubky: &[u8; 32]) -> &'a pubky_switch_client::testing::TestSigner {
    [&w.alice, &w.bob, &w.carol]
        .into_iter()
        .find(|signer| signer.identity().public() == *pubky)
        .unwrap()
}

/// Put the acceptance from [`own_acceptance_recorded`] in the recipient's inbox for each
/// app holding `paykit_allowance`. The SDK holds every delivery of an acceptance.
async fn accept_elsewhere(w: &World, app: &mut Paykit, record: &[u8; 32]) -> StoredMessage {
    let acceptance = Box::pin(own_acceptance_recorded(app, record)).await;
    let to = signer_of(w, &acceptance.message().to);
    let doc = to.switch().identity_doc().unwrap();
    let targets: Vec<[u8; 16]> = doc
        .body()
        .apps
        .iter()
        .map(|listed| listed.app_id)
        .filter(|id| {
            to.switch().app(id).cert.is_some_and(|cert| {
                cert.scopes()
                    .iter()
                    .any(|scope| scope == "paykit_allowance")
            })
        })
        .collect();
    to.switch()
        .send_plaintext(
            app.identity(),
            app.app().cert(),
            acceptance.plaintext(),
            &targets,
        )
        .unwrap();
    acceptance
}

/// Put the counterparty's `acceptance` in `record` for a test that needs it there.
/// On receipt, the SDK keeps an acceptance unplaced if it would fill an empty `response`
/// (Paykit spec 8.3.1).
async fn place_elsewhere(
    w: &World,
    app: &mut Paykit,
    record: &[u8; 32],
    acceptance: &StoredMessage,
) {
    let own = app.identity();
    let value = Box::pin(pending_at(app, record)).await;
    let received = StoredMessage::new(
        acceptance.sender(),
        acceptance.cert().signed(),
        acceptance.plaintext().clone(),
        Some(w.clock.now_ms()),
        &own,
    )
    .unwrap();
    Box::pin(install_accepted_record(app, record, value, received)).await;
}

/// Give both test records the same acceptance and put it in the proposer's inbox
/// ([`accept_elsewhere`], [`place_elsewhere`]). Return the acceptance.
async fn accepted(
    w: &World,
    acceptor: &mut Paykit,
    mine: &[u8; 32],
    proposer: &mut Paykit,
    theirs: &[u8; 32],
) -> StoredMessage {
    let acceptance = Box::pin(accept_elsewhere(w, acceptor, mine)).await;
    Box::pin(place_elsewhere(w, proposer, theirs, &acceptance)).await;
    acceptance
}

#[tokio::test]
async fn test_allowances_proposal_binds_verified_own_generation() {
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let generation = wallet.app().own_identity().generation();
    let sent =
        Box::pin(wallet.propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new()))
            .await
            .unwrap();
    let record = Box::pin(pending_at(&wallet, &sent.record)).await;
    let paykit_lib::Body::AllowanceProposal(body) = record.proposal().body() else {
        panic!("proposal")
    };
    assert_eq!(body.allower_generation, generation);
    assert_eq!(record.proposal().sender(), w.alice_id());
}

#[tokio::test]
async fn test_allowances_proposal_binds_verified_remote_generation() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let _wallet = Box::pin(wallet(&w)).await;
    let old = w.homeserver.identity_doc(&w.alice_id()).unwrap();
    w.alice
        .sign_next(|body| {
            body.service_keys
                .push(pubky_switch_core::records::ServiceKeyEntry {
                    generation: old.generation(),
                    key: old.body().inbox.service_key,
                });
            body.generation = old.generation() + 1;
        })
        .unwrap();
    let current = w.homeserver.identity_doc(&w.alice_id()).unwrap();
    assert_ne!(current.generation(), shop.app().own_identity().generation());
    let sent = Box::pin(shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms()))
        .await
        .unwrap();
    let record = Box::pin(pending_at(&shop, &sent.record)).await;
    let paykit_lib::Body::AllowanceProposal(body) = record.proposal().body() else {
        panic!("proposal")
    };
    assert_eq!(body.allower_generation, current.generation());
    assert_eq!(record.proposal().message().to, w.alice_id());
}

#[tokio::test]
async fn test_allowances_proposal_viewed_on_both_sides() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Proposed);
    assert_eq!(view.role, Some(AllowanceRole::Allower));
    assert!(!view.proposed_here);
    assert_eq!(view.counterparty, w.bob_id());
    assert_eq!(view.terms, Some(terms()));
    let sent = shop.allowance(&theirs).await.unwrap();
    assert_eq!(sent.role, Some(AllowanceRole::Allowee));
    assert!(sent.proposed_here);
    assert_eq!(sent.allowance_id, view.allowance_id);
}

async fn acceptance_refused(
    w: &World,
    app: &mut Paykit,
    signer: &pubky_switch_client::testing::TestSigner,
    record: &[u8; 32],
    other: &mut Paykit,
) {
    let writes = w.record_writes(signer, record);
    let local = app.app().protocol_scan("paykit", b"").await.unwrap();
    signer
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: format!("records/{}", b64_of(record)),
            after: 0,
        });
    let error = app.accept_allowance(record).await.unwrap_err();
    assert!(
        is_not_now(&error)
            || matches!(
                error,
                PaykitError::Client(pubky_switch_client::app::AppError::Transport(_))
            ),
        "{error:?}"
    );
    assert_eq!(w.record_writes(signer, record), writes);
    assert_eq!(app.app().protocol_scan("paykit", b"").await.unwrap(), local);
    assert_eq!(
        app.allowance(record).await.unwrap().status,
        AllowanceStatus::Proposed
    );
    assert_eq!(
        Box::pin(taken(other, "paykit.allowance_acceptance")).await,
        0
    );
}

#[tokio::test]
async fn test_allowances_acceptance_waits_for_the_current_proposal_in_either_role() {
    // Both roles need a current signed proposal before acceptance can write or queue a reply.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    // Bob proposes as allowee: alice would accept as allower.
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    // Alice proposes as allower: bob would accept as allowee.
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let to_bob = shop
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|v| v.allowance_id == sent.allowance_id)
        .unwrap();
    assert_eq!(to_bob.role, Some(AllowanceRole::Allowee));
    Box::pin(acceptance_refused(
        &w,
        &mut wallet,
        &w.alice,
        &mine,
        &mut shop,
    ))
    .await;
    Box::pin(acceptance_refused(
        &w,
        &mut shop,
        &w.bob,
        &to_bob.record,
        &mut wallet,
    ))
    .await;
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Proposed
    );
}

/// A first acceptance stays in custody when the service cannot confirm the
/// permanent record at which the signed pair would be created.
async fn receive_without_allowance_view(
    w: &World,
    app: &mut Paykit,
    record: &[u8; 32],
) -> paykit_sdk::ReceiveSummary {
    let path = format!("records/{}", b64_of(record));
    let before = w.router.log().len();
    signer_of(w, &app.identity()).switch().fail_next(
        pubky_switch_client::testing::Fault::Unreachable {
            path: path.clone(),
            after: 0,
        },
    );
    let summary = app.receive(None).await.unwrap();
    assert!(w.router.log()[before..]
        .iter()
        .any(|request| request.contains(&path)));
    summary
}

#[tokio::test]
async fn test_allowances_received_acceptance_kept_unplaced_then_given_up_with_a_notice() {
    // While the shop cannot verify the permanent record, it keeps the acceptance
    // until its 30-day window ends (Paykit spec 8.1, 8.3.1).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let acceptance_id = wallet
        .accept_executable_allowance(&mine, UserConfirmed::new())
        .await
        .unwrap();
    let proposal = Box::pin(pending_at(&shop, &theirs)).await;
    let pending = pending_key(&shop, &w.alice_id(), proposal.allowance_id());
    let before = Box::pin(raw(&shop, &pending)).await;
    let writes = w.record_writes(&w.bob, &theirs);
    let got = Box::pin(receive_without_allowance_view(&w, &mut shop, &theirs)).await;
    assert!(received(&got, "paykit.allowance_acceptance"), "{got:?}");
    for _ in 0..2 {
        assert_eq!(w.record_writes(&w.bob, &theirs), writes);
        assert!(shop.app().shared_record(&theirs).await.unwrap().is_none());
        assert_eq!(Box::pin(raw(&shop, &pending)).await, before);
        assert_eq!(
            shop.app()
                .protocol_scan("paykit", b"pr/")
                .await
                .unwrap()
                .len(),
            1
        );
        let view = shop.allowance(&theirs).await.unwrap();
        assert_eq!(view.status, AllowanceStatus::Proposed);
        assert!(!view.consented);
        assert!(shop.notices().await.unwrap().is_empty());
        w.clock.advance_ms(HOUR_MS);
        Box::pin(receive_without_allowance_view(&w, &mut shop, &theirs)).await;
    }
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &theirs), writes);
    assert!(shop.app().shared_record(&theirs).await.unwrap().is_none());
    assert_eq!(Box::pin(raw(&shop, &pending)).await, before);
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.alice_id()));
    assert_eq!(notices[0].msg_id.as_deref(), Some(acceptance_id.as_str()));
}

#[tokio::test]
async fn test_allowances_accepted_record_written_elsewhere_reports_no_authority() {
    // Alice's record already holds the acceptance. Her wallet shows the Allowance as
    // accepted, with no executor or consent, and makes no write to the record
    // (Paykit spec 8.3.1).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    let writes = w.record_writes(&w.alice, &mine);
    wallet.receive(None).await.unwrap();
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Accepted);
    assert_eq!(view.executor_app_id, None);
    assert!(!view.consented);
    assert_eq!(w.record_writes(&w.alice, &mine), writes);
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ac/")
        .await
        .unwrap()
        .is_empty());
}

/// That `app` sends no Allowance acceptance to bob's `shop`: its outbox holds none to send,
/// and the shop's next receive takes none.
async fn sends_no_acceptance(app: &mut Paykit, shop: &mut Paykit, step: &str) {
    let queued = app.app().queued_messages("paykit").await.unwrap();
    assert!(
        queued
            .iter()
            .all(|m| m.kind != "paykit.allowance_acceptance"),
        "{step}: {queued:?}"
    );
    assert_eq!(
        Box::pin(taken(shop, "paykit.allowance_acceptance")).await,
        0,
        "{step}"
    );
}

/// Alice's keeper holds bob's proposal, and her record of it holds an acceptance the keeper
/// signed, as an app of another implementation of hers records it, never sent
/// ([`own_acceptance_recorded`]). The keeper is backed up and restored; with `how`, sends to
/// bob are held at its first reads, then released. At no step does the acceptance go out or
/// even wait in the outbox: this app sends no acceptance (Paykit spec 8.3.1). The record
/// stays as it was, and shows the Allowance accepted with no executor and no consent.
async fn own_acceptance_through_a_restore(how: Option<SendsHeld>) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(own_acceptance_recorded(&mut wallet, &mine)).await;
    let before = Box::pin(raw(&wallet, &mine)).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restored_unreceived(&w, &backup)).await;
    match how {
        Some(SendsHeld::Blocked) => Box::pin(block_during_restore(&mut wallet, &w.bob_id())).await,
        Some(SendsHeld::Unknown) => fail_list_read(&w),
        None => {}
    }
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        Box::pin(sends_no_acceptance(&mut wallet, &mut shop, "restored")).await;
    }
    if let Some(how) = how {
        Box::pin(release_sends(&w, &mut wallet, how)).await;
        for _ in 0..2 {
            wallet.receive(None).await.unwrap();
            Box::pin(sends_no_acceptance(&mut wallet, &mut shop, "released")).await;
        }
    }
    Box::pin(read_restored_records(&mut wallet)).await;
    Box::pin(sends_no_acceptance(&mut wallet, &mut shop, "listed")).await;
    assert_eq!(Box::pin(raw(&wallet, &mine)).await, before);
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Accepted);
    assert_eq!(view.executor_app_id, None);
    assert!(!view.consented);
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Proposed
    );
}

#[tokio::test]
async fn test_allowances_restore_sends_no_own_acceptance_from_its_record() {
    Box::pin(own_acceptance_through_a_restore(None)).await;
}

#[tokio::test]
async fn test_allowances_restore_sends_no_own_acceptance_once_a_block_is_lifted() {
    Box::pin(own_acceptance_through_a_restore(Some(SendsHeld::Blocked))).await;
}

#[tokio::test]
async fn test_allowances_restore_sends_no_own_acceptance_once_the_deny_list_is_known() {
    Box::pin(own_acceptance_through_a_restore(Some(SendsHeld::Unknown))).await;
}

#[tokio::test]
async fn test_allowances_acceptance_in_the_outbox_never_delivered() {
    // An acceptance the wallet signed waits in its outbox, as one a restored outbox may
    // carry. The delivery gate, asked before every try of a delivery, holds it at a flush of
    // the outbox, a flush of that message and a receive: bob takes nothing (Paykit spec
    // 8.3.1). Other messages still go.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let acceptance = Box::pin(own_acceptance_recorded(&mut wallet, &mine)).await;
    let msg_id = wallet
        .app_mut()
        .queue_stored(acceptance.plaintext())
        .await
        .unwrap();
    wallet.app_mut().flush_outbox().await.unwrap();
    let _ = wallet.app_mut().flush_message(&msg_id).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_acceptance")).await,
        0
    );
    let queued = wallet.app().queued_messages("paykit").await.unwrap();
    assert!(queued.iter().any(|m| m.msg_id == msg_id), "{queued:?}");
    wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        1
    );
}

#[tokio::test]
async fn test_allowances_answer_refused_to_the_proposer_and_once_answered() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    // Only the proposal's recipient answers it.
    let error = shop.reject_allowance(&theirs).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    wallet.reject_allowance(&mine).await.unwrap();
    // The first rejection removed the pending record.
    let error = wallet.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::NotFound)),
        "{error:?}"
    );
    // An app without `paykit_allowance` proposes nothing.
    let mut plain = w.wallet("plain").await;
    let error = plain
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap_err();
    let Some(missing @ Refusal::ScopeMissing(scope)) = refusal(&error) else {
        panic!("{error:?}")
    };
    assert_eq!(scope, "paykit_allowance");
    assert_eq!(
        missing.to_string(),
        "Paykit spec 3.1: the certificate doesn't hold paykit_allowance"
    );
}

#[tokio::test]
async fn test_allowances_rejected_one_has_no_end() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    wallet.reject_allowance(&mine).await.unwrap();
    shop.receive(None).await.unwrap();
    for (app, record) in [(&mut shop, theirs), (&mut wallet, mine)] {
        assert!(matches!(
            app.allowance(&record).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
        assert!(matches!(
            app.end_allowance(&record).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
    }
}

#[tokio::test]
async fn test_allowances_withdrawal_before_an_answer_ends_it() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    // The recipient doesn't end a proposal it hasn't answered: it rejects it.
    let error = wallet.end_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    shop.end_allowance(&theirs).await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut wallet, "paykit.allowance_end")).await,
        1
    );
    for (app, record) in [(&mut shop, theirs), (&mut wallet, mine)] {
        assert!(matches!(
            app.allowance(&record).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
        assert!(matches!(
            app.reject_allowance(&record).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
        assert!(matches!(
            app.accept_allowance(&record).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
    }
}

#[tokio::test]
async fn test_allowances_counterparty_end_kept_as_the_stop_then_recorded() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accepted(&w, &mut wallet, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    let writes = w.record_writes(&w.alice, &mine);
    wallet.receive(None).await.unwrap();
    // Recorded once by the wallet's own later write; the kept stop then goes.
    assert_eq!(w.record_writes(&w.alice, &mine), writes + 1);
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    let end = Box::pin(value(&wallet, &mine)).await["end"].clone();
    assert!(!end.is_null());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    let error = wallet.end_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceEnded)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_allowances_end_naming_another_proposal_dropped() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    let view = wallet.allowance(&mine).await.unwrap();
    // An end of this Allowance's ID that refers to another proposal isn't its end.
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![[7; 32], [8; 32]],
        json!({ "allowance_id": view.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    // Dropped at the receive step: no stop is kept from it, not even for a moment.
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.allowance_end"), "{summary:?}");
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_allowances_withdrawal_crossing_the_acceptance_ends_it_on_both_sides() {
    use paykit_lib::allowances::Ledger;

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let pending = Box::pin(pending_at(&wallet, &mine)).await;
    // The shop withdraws. The payer approves the original before reading that message.
    let withdrawal = shop.end_allowance(&theirs).await.unwrap();
    let acceptance = wallet
        .accept_executable_allowance(&mine, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    let ledgers = wallet.app().records_of("allowance_ledgers").await.unwrap();
    let [original] = ledgers.as_slice() else {
        panic!("the payer created one ledger for its acceptance");
    };
    let incoming = Box::pin(received_originals(&mut wallet, &[&withdrawal])).await;
    assert_eq!(incoming, std::slice::from_ref(&withdrawal));
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Ended);
    assert!(view.executor_app_id.is_none() && !view.consented);

    let closed = wallet.app().shared_record(&mine).await.unwrap().unwrap();
    let record = AllowanceRecord::read(closed.content.value().unwrap(), &w.alice_id()).unwrap();
    let q = record.response.as_ref().unwrap();
    assert_eq!(record.proposal.hash(), pending.proposal().hash());
    assert_eq!(q.message().msg_id, acceptance);
    assert_eq!(q.message().from.app_id, wallet.app_id());
    assert_eq!(q.message().refs, [record.proposal.hash()]);
    assert_eq!(
        original.content.value().unwrap(),
        Ledger::initial_value(&w.bob_id(), pending.allowance_id(), &q.hash())
    );
    let ledger = wallet
        .app()
        .shared_record(&original.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ledger.version, original.version);
    assert_eq!(ledger.content, original.content);
    let end = record.end.as_ref().unwrap();
    assert_eq!(end.message().from.app_id, wallet.app_id());
    assert_eq!(end.message().refs, [record.proposal.hash(), q.hash()]);
    assert!(record.msg_ids.contains(&withdrawal));
    let end_id = &end.message().msg_id;
    let delivered = Box::pin(received_originals(&mut shop, &[&acceptance, end_id])).await;
    assert_eq!(delivered, [acceptance.as_str(), end_id.as_str()]);
    assert!(matches!(
        shop.allowance(&theirs).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    assert!(shop.allowances().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowances_withdrawal_read_before_approval_leaves_the_ledger_unopened() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let pending = Box::pin(pending_at(&wallet, &mine)).await;
    let key = pending_key(&wallet, &w.bob_id(), pending.allowance_id());
    let withdrawal = shop.end_allowance(&theirs).await.unwrap();
    let delivered = Box::pin(received_originals(&mut wallet, &[&withdrawal])).await;
    assert_eq!(delivered, [withdrawal]);
    assert!(Box::pin(held_deleted(&wallet, &key)).await);
    let refused = wallet
        .accept_executable_allowance(&mine, UserConfirmed::new())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::NotFound));
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(wallet
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());
    assert!(!received(
        &shop.receive(None).await.unwrap(),
        "paykit.allowance_acceptance"
    ));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowances_end_for_a_record_not_listed_yet_is_held_until_listed() {
    // Alice's phone accepts bob's proposal; the wallet, holding `paykit_pay` only, can't read the
    // record. The wallet is renewed to read `paykit_allowance` too, with no bundle, and bob ends
    // the Allowance. The record's absence from the wallet's copy decides nothing: the end's
    // entry is held, the cursor before it, and no stop is kept from it as new state. Once
    // the keys come and the records are listed, the end is taken once and recorded.
    let w = World::new();
    let mut wallet = Box::pin(alice_app(
        &w,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
    ))
    .await;
    let me = wallet.app_id();
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut phone)).await;
    Box::pin(accepted(&w, &mut phone, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().allowance().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let before = wallet.receive(None).await.unwrap().report.cursor;
    // Bob's shop reads alice's directory again, which lists the wallet under its new
    // certificate, so the end goes to it too.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.allowance_end"), "{summary:?}");
    assert_eq!(summary.report.cursor, before);
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    let unlisted = wallet.allowances().await.unwrap_err();
    assert!(is_not_now(&unlisted), "{unlisted:?}");
    store_bundle(&w, me, &wider);
    let mut ends = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        ends += usize::from(received(&summary, "paykit.allowance_end"));
    }
    assert_eq!(ends, 1);
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(!Box::pin(value(&wallet, &mine)).await["end"].is_null());
}

#[tokio::test]
async fn test_allowances_answer_before_its_record_kept_then_given_up() {
    // An acceptance of a proposal this identity's app holds no record of is kept, as new
    // state, not dropped; with no record coming, it goes 30 days after its acceptance.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let _wallet = Box::pin(wallet(&w)).await;
    let proposal = [7; 32];
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.allowance_acceptance",
        vec![proposal],
        json!({ "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4" })
            .as_object()
            .cloned()
            .unwrap(),
    );
    let summary = shop.receive(None).await.unwrap();
    assert!(
        received(&summary, "paykit.allowance_acceptance"),
        "{summary:?}"
    );
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(shop.allowances().await.unwrap().is_empty());
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
}

/// Alice's executor app, holding `paykit_allowance_execute`, writes its grant into the accepted
/// Allowance at `record` and puts it on its consent list, as the executor does at
/// acceptance.
async fn grant(w: &World, exec: &mut Paykit, record: &[u8; 32]) {
    let held = exec.app().shared_record(record).await.unwrap().unwrap();
    let mut value = AllowanceRecord::read(held.content.value().unwrap(), &w.alice_id()).unwrap();
    let keys = w.alice.identity().app_keys("exec");
    let signed = ExecutorGrant::sign(
        &keys,
        exec.app().cert().signed().clone(),
        &w.alice_id(),
        &value.counterparty,
        &value.allowance_id,
        value.terms_hash().unwrap(),
        1,
        held.version,
    );
    value.grants.push(GrantEntry {
        built_on: held.version,
        grant: signed.clone(),
        origin: Default::default(),
    });
    value.grant = Some(signed);
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(&value.counterparty, &value.allowance_id),
    );
    let op = exec
        .app_mut()
        .record_op(&target, Some(&value.to_json().unwrap()), Vec::new())
        .await
        .unwrap();
    exec.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    Box::pin(consent(exec, record)).await;
}

/// The consent list's entry of `record`, held and confirmed, as the executor's grant
/// writes it: of the Allowance the record holds.
async fn consent(app: &mut Paykit, record: &[u8; 32]) {
    let held = app.app().shared_record(record).await.unwrap().unwrap();
    let value = AllowanceRecord::read(held.content.value().unwrap(), &app.identity()).unwrap();
    let key = format!("ac/{}", pubky_switch_core::enc::b64(record));
    let entry = json!({
        "v": 1,
        "d": {
            "counterparty": pubky_switch_core::enc::b64(&value.counterparty),
            "allowance_id": value.allowance_id.as_str(),
            "consent": { "held": { "confirm_again": false } },
        },
    });
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.into_bytes(),
                value: serde_json::to_vec(&entry).unwrap(),
            }],
        )
        .await
        .unwrap();
}

/// The consent list's entry of `record` at `app`, as stored.
async fn consent_entry(app: &Paykit, record: &[u8; 32]) -> Option<Value> {
    let key = format!("ac/{}", pubky_switch_core::enc::b64(record));
    app.app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .map(|v| serde_json::from_slice::<Value>(&v).unwrap()["d"]["consent"].clone())
}

/// Bob's Allowance accepted by alice's wallet, with her `exec` app as its executor, and
/// her `phone`, another executor-capable app. Returns the apps and alice's record key.
async fn executed(w: &World) -> (Paykit, Paykit, Paykit, Paykit, [u8; 32], [u8; 32]) {
    let mut shop = Box::pin(shop(w)).await;
    let mut wallet = Box::pin(wallet(w)).await;
    let executor = AppScopes::new().allowance_execute().build().unwrap();
    let mut exec = Box::pin(alice_app(w, "exec", executor.clone())).await;
    let mut phone = Box::pin(alice_app(w, "phone", executor)).await;
    let (theirs, mine) = Box::pin(proposed(w, &mut shop, &mut wallet)).await;
    Box::pin(accepted(w, &mut wallet, &mine, &mut shop, &theirs)).await;
    exec.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    Box::pin(grant(w, &mut exec, &mine)).await;
    Box::pin(consent(&mut phone, &mine)).await;
    wallet.receive(None).await.unwrap();
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.executor_app_id, Some(exec.app_id()));
    assert!(exec.allowance(&mine).await.unwrap().consented);
    (shop, wallet, exec, phone, theirs, mine)
}

#[tokio::test]
async fn test_allowances_end_notifies_the_executor_only() {
    let w = World::new();
    let (_shop, mut wallet, mut exec, mut phone, _, mine) = Box::pin(executed(&w)).await;
    // The notice is wrapped to the executor the grant names, not to the phone.
    wallet.end_allowance(&mine).await.unwrap();
    let summary = exec.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.executor_notice"), "{summary:?}");
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    assert!(!exec.allowance(&mine).await.unwrap().consented);
    let summary = phone.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.executor_notice"), "{summary:?}");
    // The phone still drops it, for good, once it sees the record ended.
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_counterparty_end_recorded_with_a_notice() {
    let w = World::new();
    let (mut shop, mut wallet, mut exec, _phone, theirs, mine) = Box::pin(executed(&w)).await;
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    // The executor reads its own inbox first: the notice isn't there yet, the end is.
    wallet.receive(None).await.unwrap();
    let summary = exec.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.executor_notice"), "{summary:?}");
    assert!(!exec.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_consent_kept_while_accepted_and_dropped_once_ended() {
    let w = World::new();
    let (mut shop, _wallet, _exec, mut phone, theirs, mine) = Box::pin(executed(&w)).await;
    phone.receive(None).await.unwrap();
    assert!(phone.allowance(&mine).await.unwrap().consented);
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    // The phone takes bob's end: its consent goes for good in the commit that keeps the stop.
    phone.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    assert!(!phone.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_restore_asks_consent_again_and_keeps_removals() {
    Box::pin(restored_consent(false)).await;
}

#[tokio::test]
async fn test_allowances_clean_restore_keeps_consent_and_removals() {
    Box::pin(restored_consent(true)).await;
}

async fn restored_consent(clean: bool) {
    let w = World::new();
    let (_shop, mut wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    let backup = exec.backup().await.unwrap();
    let id = exec.app_id();
    drop(exec);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(restored.app_id(), id);
    if !clean {
        crate::common::restore_with_unknown_cursor(&mut restored).await;
    }
    Box::pin(read_restored_records(&mut restored)).await;
    let expected = if clean {
        pubky_switch_client::app::HeldCursor::Clean
    } else {
        pubky_switch_client::app::HeldCursor::Unknown
    };
    assert_eq!(restored.app().restore_held_cursor(), Some(expected));
    assert_eq!(
        Box::pin(consent_entry(&restored, &mine)).await,
        Some(json!({ "held": { "confirm_again": !clean } }))
    );
    assert_eq!(restored.allowance(&mine).await.unwrap().consented, clean);
    // A removal stays removed across a restore.
    wallet.end_allowance(&mine).await.unwrap();
    restored.receive(None).await.unwrap();
    let backup = restored.backup().await.unwrap();
    drop(restored);
    let mut again = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    Box::pin(read_restored_records(&mut again)).await;
    assert_eq!(
        Box::pin(consent_entry(&again, &mine)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_app_without_allowance_takes_none_of_the_messages() {
    // An app whose certificate lacks `paykit_allowance` lists none of the kinds, so bob's proposal
    // goes only to the app that takes it, and the other writes no Allowance record.
    let w = World::new();
    let mut plain = w.wallet("plain").await;
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let summary = plain.receive(None).await.unwrap();
    assert!(
        !received(&summary, "paykit.allowance_proposal"),
        "{summary:?}"
    );
    assert!(plain.allowances().await.unwrap().is_empty());
    let view = wallet.allowance(&mine).await.unwrap();
    let pending = pending_key(&wallet, &w.bob_id(), &view.allowance_id);
    assert_eq!(w.record_writes(&w.alice, &pending), 1);
    assert_eq!(w.record_writes(&w.alice, &mine), 0);
}

#[tokio::test]
async fn test_allowances_kept_stop_refuses_an_answer_before_it_is_recorded() {
    // Bob's end names an acceptance alice's record doesn't hold: it can't be recorded yet,
    // and is kept as the stop. Nothing is answered under it meanwhile.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let record = Box::pin(pending_at(&wallet, &mine)).await;
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![record.proposal().hash(), [8; 32]],
        json!({ "allowance_id": record.allowance_id().as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        record.proposal().hash()
    );
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    let error = wallet.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceEnded)),
        "{error:?}"
    );
}

/// The re-sends `app` asked of bob.
async fn asked_bob(w: &World, app: &Paykit) -> usize {
    crate::common::asked(app, &w.bob_id()).await.len()
}

/// Alice's wallet restored from a backup taken while it held bob's Allowance, accepted and,
/// with `ended`, ended since; the restored app's first read done.
async fn restored_with_allowance(w: &World, ended: bool) -> Paykit {
    let mut shop = Box::pin(shop(w)).await;
    let mut wallet = Box::pin(wallet(w)).await;
    let (_, mine) = Box::pin(proposed(w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(w, &mut wallet, &mine)).await;
    if ended {
        wallet.end_allowance(&mine).await.unwrap();
    }
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.receive(None).await.unwrap();
    restored
}

#[tokio::test]
async fn test_allowances_restore_asks_the_counterparty_of_an_open_one_to_resend() {
    // The backup may predate bob's end: he is asked to send again.
    let w = World::new();
    let restored = Box::pin(restored_with_allowance(&w, false)).await;
    assert!(Box::pin(asked_bob(&w, &restored)).await > 0);
}

#[tokio::test]
async fn test_allowances_restore_asks_nothing_for_an_ended_one() {
    let w = World::new();
    let restored = Box::pin(restored_with_allowance(&w, true)).await;
    assert_eq!(Box::pin(asked_bob(&w, &restored)).await, 0);
}

/// An `executor_notice` with `reason` naming `record`, from `signer`'s app `from` to alice's
/// app `to`.
fn notice(
    w: &World,
    signer: &pubky_switch_client::testing::TestSigner,
    from: &str,
    to: [u8; 16],
    record: &[u8; 32],
    reason: &str,
) {
    w.inject(
        signer,
        from,
        &w.alice,
        &[to],
        "paykit.executor_notice",
        Vec::new(),
        json!({
            "allowance_record": pubky_switch_core::enc::b64(record),
            "reason": reason
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
}

#[tokio::test]
async fn test_allowances_executor_notice_removes_consent_for_good() {
    // A handoff's notice from an own app changes only the consent list: the Allowance stays
    // accepted, and this executor no longer pays under it.
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    notice(&w, &w.alice, "wallet", exec.app_id(), &mine, "handed_off");
    let summary = exec.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.executor_notice"), "{summary:?}");
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "removed": { "reason": "handed_off" } }))
    );
    let view = exec.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Accepted);
    assert!(!view.consented);
}

#[tokio::test]
async fn test_allowances_executor_notice_from_another_identity_dropped() {
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    notice(&w, &w.bob, "shop", exec.app_id(), &mine, "ended");
    let summary = exec.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.executor_notice"), "{summary:?}");
    assert!(exec.allowance(&mine).await.unwrap().consented);
}

/// The next `n` batches at alice's switch are refused for good, `403 permission_missing`.
fn refuse_writes(w: &World, n: usize) {
    for _ in 0..n {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: Value::Null,
            });
    }
}

#[tokio::test]
async fn test_allowances_deferred_proposal_is_written_once_after_a_refusal() {
    // The wallet retains the signed proposal when its first create is refused.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert!(wallet.allowances().await.unwrap().is_empty());
    assert!(Box::pin(keeps(&wallet, b"pr/", &sent.msg_id)).await);
    let key = wallet
        .app()
        .record_key(&pending_target(&w.bob_id(), &sent.allowance_id))
        .unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), 0);
    wallet.receive(None).await.unwrap();
    let views = wallet.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), 1);
    assert!(!Box::pin(keeps(&wallet, b"pr/", &sent.msg_id)).await);
    assert_eq!(views[0].status, AllowanceStatus::Proposed);
}

/// Bob's Allowance, proposed to alice's `wallet` and `phone` and accepted by the phone, whose
/// record the wallet holds sealed under a key it doesn't hold yet: the phone wrote the
/// acceptance under an epoch's key the wallet never got, and the wallet holds only a later
/// one. Bob's shop has taken the acceptance. Returns the record keys on bob's side and on
/// alice's, and the wallet's certificate, for the key bundle that opens the record.
async fn sealed_allowance(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    phone: &mut Paykit,
) -> ([u8; 32], [u8; 32], pubky_switch_core::records::AppCert) {
    let (theirs, mine) = Box::pin(proposed(w, shop, wallet)).await;
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    Box::pin(accepted(w, phone, &mine, shop, &theirs)).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(w, wallet.app_id(), &cert, newest);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&mine).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::KeyNotHeld { .. }
            )
        ),
        "{:?}",
        held.content
    );
    shop.receive(None).await.unwrap();
    (theirs, mine, cert)
}

/// A wallet and a phone of alice's, both holding `paykit_pay` and `paykit_allowance`, and bob's
/// shop.
async fn two_wallets(w: &World) -> (Paykit, Paykit, Paykit) {
    Box::pin(two_wallets_with(
        w,
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await
}

async fn two_wallets_with(w: &World, scopes: RequestedScopes) -> (Paykit, Paykit, Paykit) {
    let shop = Box::pin(shop(w)).await;
    let wallet = Box::pin(alice_app(w, "wallet", scopes.clone())).await;
    let phone = Box::pin(alice_app(w, "phone", scopes)).await;
    (shop, wallet, phone)
}

#[tokio::test]
async fn test_allowances_end_while_its_record_awaits_its_key_stops_it_past_30_days() {
    // The wallet holds the Allowance's record sealed under a key it doesn't hold yet. Bob
    // ends it. The stop is kept at once, the entry waiting for no key, and 30 days on no
    // give-up takes it: once the key comes, the record opens and the wallet records the end
    // (Paykit spec 8.3). The phone never reads its inbox again, so only the wallet's stop
    // can end it.
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = Box::pin(two_wallets(&w)).await;
    let (theirs, mine, cert) =
        Box::pin(sealed_allowance(&w, &mut shop, &mut wallet, &mut phone)).await;
    shop.end_allowance(&theirs).await.unwrap();
    let got = wallet.receive(None).await.unwrap();
    assert!(got.key_epochs_awaited.is_empty(), "{got:?}");
    assert!(received(&got, "paykit.allowance_end"), "{got:?}");
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(wallet.notices().await.unwrap().is_empty());
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(!Box::pin(value(&wallet, &mine)).await["end"].is_null());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_allowances_end_kept_while_sealed_goes_once_it_opens_naming_another_proposal() {
    // An end kept at once over a sealed record is checked once the record opens: one that
    // refers to another proposal isn't this Allowance's end, and its stop goes, the Allowance
    // still accepted (Paykit wire 4.16).
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = Box::pin(two_wallets(&w)).await;
    let (_, mine, cert) = Box::pin(sealed_allowance(&w, &mut shop, &mut wallet, &mut phone)).await;
    let allowance_id = phone.allowance(&mine).await.unwrap().allowance_id;
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![[7; 32], [8; 32]],
        json!({ "allowance_id": allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    let got = wallet.receive(None).await.unwrap();
    assert!(got.key_epochs_awaited.is_empty(), "{got:?}");
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    assert!(Box::pin(value(&wallet, &mine)).await["end"].is_null());
}

/// Bob's end of an Allowance with ID number `i` that alice's `wallet` holds no record of.
fn end_unknown(w: &World, wallet: &Paykit, i: usize) {
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![[0xee; 32]],
        json!({ "allowance_id": format!("d0efffb7-191b-4709-b6c7-e343af55{i:04x}") })
            .as_object()
            .cloned()
            .unwrap(),
    );
}

/// The stops `wallet` keeps, its kept cancellations and Allowance ends together.
async fn stops(wallet: &Paykit) -> usize {
    let mut n = 0;
    for prefix in [&b"kc/"[..], &b"ae/"[..]] {
        n += wallet
            .app()
            .protocol_scan("paykit", prefix)
            .await
            .unwrap()
            .len();
    }
    n
}

/// Alice's record of bob's Allowance with ID number `i`.
fn allowance_record(w: &World, i: usize) -> pubky_switch_client::app::RecordTarget {
    let id =
        paykit_lib::AllowanceId::new(&format!("d0efffb7-191b-4709-b6c7-e343af55{i:04x}")).unwrap();
    pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    )
}

/// Whether `wallet` keeps an end of bob's of the Allowance with ID number `i` as a stop.
async fn keeps_end(w: &World, wallet: &Paykit, i: usize) -> bool {
    let target = allowance_record(w, i);
    let key = wallet.app().record_key(&target).unwrap();
    Box::pin(keeps_end_at(wallet, &key)).await
}

/// How many ends `wallet` keeps as the stops of the Allowance whose record is `record`.
async fn ends_kept_at(wallet: &Paykit, record: &[u8; 32]) -> usize {
    let stops = format!("ae/{}/", pubky_switch_core::enc::b64(record));
    wallet
        .app()
        .protocol_scan("paykit", stops.as_bytes())
        .await
        .unwrap()
        .len()
}

/// Whether `wallet` keeps an end as the stop of the Allowance whose record is `record`.
async fn keeps_end_at(wallet: &Paykit, record: &[u8; 32]) -> bool {
    Box::pin(ends_kept_at(wallet, record)).await > 0
}

#[tokio::test]
async fn test_allowances_stops_for_allowances_not_held_bounded_with_cancellations() {
    // Bob cancels half of 64 requests the wallet holds no record of and ends as many
    // Allowances it holds none of, a millisecond apart: the 64 stops are kept. One more end
    // drops the earliest of them, a cancellation, since the two kinds of stop count under one
    // bound per sender (Paykit spec 8.1, 8.3).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let half = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER / 2;
    let unknown = |i: usize| [u8::try_from(i + 1).unwrap(); 32];
    for i in 0..half {
        w.clock.advance_ms(1);
        crate::one_time::cancel_to(&w, &[wallet.app_id()], unknown(i));
    }
    for i in 0..half {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    wallet.receive(None).await.unwrap();
    let all = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    assert_eq!(Box::pin(stops(&wallet)).await, all);
    w.clock.advance_ms(1);
    end_unknown(&w, &wallet, half);
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, all);
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"kc/")
            .await
            .unwrap()
            .len(),
        half - 1
    );
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
    assert!(Box::pin(keeps_end(&w, &wallet, half)).await);
}

#[tokio::test]
async fn test_allowances_stop_over_a_readable_record_counted_in_the_bound() {
    // Bob's end of an Allowance the wallet holds names an acceptance its record doesn't hold:
    // the stop is kept 30 days, under the bounds of the stops for records not held, counted
    // with them. Then 64 ends of Allowances it holds no record of, later: the 64th drops the
    // earliest of the sender's stops, the one over the record held (Paykit spec 8.1, 8.3).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(own_acceptance_recorded(&mut wallet, &mine)).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![record.proposal.hash(), [8; 32]],
        json!({ "allowance_id": record.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, 1);
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound);
    assert!(!Box::pin(keeps_end_at(&wallet, &mine)).await);
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
    assert!(Box::pin(keeps_end(&w, &wallet, bound - 1)).await);
}

/// A `missed` item at alice's switch: entries accepted in the last second were deleted before
/// her apps read them (Switch spec 8.2).
fn missed(w: &World) {
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
}

#[tokio::test]
async fn test_allowances_missed_item_asks_consent_again_until_the_user_confirms() {
    // An end may be among the entries a `missed` item tells of: every Allowance on the
    // consent list is to be confirmed again, as after a restore (Paykit spec 7.2, 8.3). The
    // user's confirmation then counts again.
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    missed(&w);
    Box::pin(crate::common::finish_listing(&mut exec)).await;
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "held": { "confirm_again": true } }))
    );
    assert!(!exec.allowance(&mine).await.unwrap().consented);
    Box::pin(consent(&mut exec, &mine)).await;
    assert!(exec.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_missed_item_keeps_removals_and_consents_nothing_before_the_marks() {
    // A removal stays removed past a `missed` item. Until the receive marks each entry to be
    // confirmed again, the item's own mark consents nothing, as after a crash between them.
    let w = World::new();
    let (_shop, _wallet, mut exec, mut phone, _, mine) = Box::pin(executed(&w)).await;
    notice(&w, &w.alice, "wallet", exec.app_id(), &mine, "handed_off");
    exec.receive(None).await.unwrap();
    missed(&w);
    exec.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "removed": { "reason": "handed_off" } }))
    );
    assert!(phone.allowance(&mine).await.unwrap().consented);
    phone
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"mc".to_vec(),
                value: br#"{"v":1,"d":true}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    assert!(!phone.allowance(&mine).await.unwrap().consented);
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "held": { "confirm_again": false } }))
    );
}

#[tokio::test]
async fn test_allowances_stored_proposal_waiting_for_the_current_epochs_key_given_up_after_30_days()
{
    // Bob's proposal is stored, and then the data epoch rotates with no key for the wallet:
    // nothing can be sealed at the record yet. The proposal waits for the key a day short of
    // 30 days from its acceptance, and is then given up, the host told once (Paykit spec 8.1).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    let day = pubky_switch_core::records::DAY_MS;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - day);
    wallet.app_mut().rotate().await.unwrap();
    wallet.app_mut().flush_cursor().await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(wallet.notices().await.unwrap().is_empty());
    w.clock.advance_ms(2 * day);
    wallet.app_mut().rotate().await.unwrap();
    wallet.app_mut().flush_cursor().await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.bob_id()));
}

/// Bob's proposal of the Allowance with ID number `i`, entered for alice's `wallet`.
fn propose_unknown(w: &World, wallet: &Paykit, i: usize) {
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_proposal",
        Vec::new(),
        json!({
            "allowance_id": format!("d0efffb7-191b-4709-b6c7-e343af55{i:04x}"),
            "proposer_role": "allowee", "allower_generation": 1,
            "terms": terms().to_json().unwrap(),
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
}

/// The wallet keeps Bob's end before receiving his proposal. Its first pending create
/// is refused, so the proposal and end are both due at the next read.
async fn stop_with_a_stored_proposal(w: &World, wallet: &mut Paykit) {
    end_unknown(w, wallet, 0);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    propose_unknown(w, wallet, 0);
    refuse_writes(w, 1);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps_end(w, wallet, 0)).await);
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_allowances_stop_kept_past_30_days_while_a_stored_message_waits() {
    // The data epoch then rotates with no key for the wallet, so the proposal can't be
    // written yet. Past 30 days from the end's acceptance, the proposal is still within its
    // own 30 days, to write. The stop stays: once the proposal opens the record, the stop
    // must be there (Paykit spec 8.1, 8.3).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    Box::pin(stop_with_a_stored_proposal(&w, &mut wallet)).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - HOUR_MS);
    wallet.app_mut().rotate().await.unwrap();
    wallet.app_mut().flush_cursor().await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
}

#[tokio::test]
async fn test_allowances_stop_with_a_stored_message_never_dropped_for_the_bound() {
    // The stop of an Allowance whose stored proposal waits counts in no bound: 64 later ends
    // of other Allowances are all kept beside it, and a 65th drops the earliest of those.
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    Box::pin(stop_with_a_stored_proposal(&w, &mut wallet)).await;
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 1..=bound + 1 {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound + 1);
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
    assert!(!Box::pin(keeps_end(&w, &wallet, 1)).await);
    assert!(Box::pin(keeps_end(&w, &wallet, bound + 1)).await);
}

/// A genuine rejection arrives before or after the phone reads the sealed proposal.
async fn sealed_pending_rejection(
    w: &World,
    rejection_first: bool,
) -> (Paykit, [u8; 32], Vec<u8>, String) {
    use paykit_lib::records::PendingAllowanceRecord;
    let (mut shop, mut writer, mut phone) = Box::pin(two_wallets_with(
        w,
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let before = if rejection_first {
        Some(
            Box::pin(rejection_before_its_pending(
                w,
                &mut phone,
                &mut shop,
                AllowanceRole::Allower,
            ))
            .await,
        )
    } else {
        None
    };
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(w, writer.app_id(), &writer.app().cert().clone());
    writer.receive(None).await.unwrap();
    writer.receive(None).await.unwrap();
    let (record, written, already_received) = match before {
        Some((proposal, id, rejection)) => {
            let target = pending_target(&w.bob_id(), &id);
            let bytes = PendingAllowanceRecord::new(&w.alice_id(), proposal)
                .unwrap()
                .to_json()
                .unwrap();
            Box::pin(write_record(&mut writer, &target, Some(&bytes))).await;
            (
                writer.app().record_key(&target).unwrap(),
                bytes,
                Some(rejection),
            )
        }
        None => {
            let sent = writer
                .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
                .await
                .unwrap();
            let key = pending_key(&writer, &w.bob_id(), &sent.allowance_id);
            (key, Box::pin(raw(&writer, &key)).await, None)
        }
    };
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    let epoch = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(w, phone.app_id(), &phone.app().cert().clone(), epoch);
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    assert_sealed_pending(&phone, &record).await;
    let rejection = if let Some(original) = already_received {
        assert!(Box::pin(keeps(&phone, b"pr/", &original)).await);
        original
    } else {
        shop.receive(None).await.unwrap();
        let theirs = shop.allowances().await.unwrap()[0].record;
        shop.reject_allowance(&theirs).await.unwrap()
    };
    (phone, record, written, rejection)
}

async fn assert_sealed_pending(phone: &Paykit, record: &[u8; 32]) {
    use pubky_switch_client::app::{RecordContent, Unopened};
    let held = phone.app().shared_record(record).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            RecordContent::Unreadable(Unopened::KeyNotHeld { .. })
        ),
        "{held:?}"
    );
}

/// The value of the record at `record` as `app` holds it, as bytes.
async fn raw(app: &Paykit, record: &[u8; 32]) -> Vec<u8> {
    let held = app.app().shared_record(record).await.unwrap().unwrap();
    held.content.value().unwrap().to_vec()
}

/// Whether `summary` shows an entry stored locally, its step to be tried again.
fn stored_locally(summary: &paykit_sdk::ReceiveSummary) -> bool {
    summary
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::Parked { .. }))
}

#[tokio::test]
async fn test_allowances_answer_over_a_record_that_doesnt_read_kept_then_written() {
    let w = World::new();
    let (mut phone, record, _, rejection) = Box::pin(sealed_pending_rejection(&w, false)).await;
    let writes = w.record_writes(&w.alice, &record);
    let got = phone.receive(None).await.unwrap();
    assert!(!received(&got, "paykit.allowance_rejection"), "{got:?}");
    assert!(!got.key_epochs_awaited.is_empty(), "{got:?}");
    assert_eq!(w.record_writes(&w.alice, &record), writes);
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    let mut taken = 0;
    for _ in 0..3 {
        let got = phone.receive(None).await.unwrap();
        taken += got
            .report
            .events
            .iter()
            .filter(
                |e| matches!(e, AppEvent::MessageReceived { msg_id, .. } if msg_id == &rejection),
            )
            .count();
    }
    assert_eq!(taken, 1);
    assert!(Box::pin(held_deleted(&phone, &record)).await);
    assert_eq!(w.record_writes(&w.alice, &record), writes + 1);
}

#[tokio::test]
async fn test_allowances_answer_over_a_record_that_doesnt_read_never_written_over_it() {
    let w = World::new();
    let (mut phone, record, _, rejection) = Box::pin(sealed_pending_rejection(&w, false)).await;
    let writes = w.record_writes(&w.alice, &record);
    let got = phone.receive(None).await.unwrap();
    assert!(!got.key_epochs_awaited.is_empty(), "{got:?}");
    assert!(phone.notices().await.unwrap().is_empty());
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..3 {
        phone.receive(None).await.unwrap();
    }
    assert_sealed_pending(&phone, &record).await;
    assert_eq!(w.record_writes(&w.alice, &record), writes);
    let notices = phone.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].msg_id.as_deref(), Some(rejection.as_str()));
}

/// Alice's phone proposes an Allowance to bob; her wallet, which takes no message from bob,
/// reads its record. With `ended`, the phone withdraws it, and the wallet reads that too.
async fn allowance_seen_by_the_wallet(w: &World, ended: bool) -> Paykit {
    let _shop = Box::pin(shop(w)).await;
    let mut phone = Box::pin(alice_app(
        w,
        "phone",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let mut wallet = Box::pin(wallet(w)).await;
    let sent = phone
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    if ended {
        phone.end_allowance(&sent.record).await.unwrap();
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowances().await.unwrap().len(),
        usize::from(!ended)
    );
    assert_eq!(Box::pin(asked_bob(w, &wallet)).await, 0);
    wallet
}

#[tokio::test]
async fn test_allowances_missed_item_asks_the_counterparty_of_an_open_one_to_resend() {
    // An end may be among the entries a `missed` item tells of: bob is asked to send again,
    // as for an open request (Paykit spec 8.3, 9).
    let w = World::new();
    let mut wallet = Box::pin(allowance_seen_by_the_wallet(&w, false)).await;
    missed(&w);
    Box::pin(crate::common::finish_listing(&mut wallet)).await;
    assert!(Box::pin(asked_bob(&w, &wallet)).await > 0);
}

#[tokio::test]
async fn test_allowances_missed_item_asks_nothing_for_an_ended_one() {
    let w = World::new();
    let mut wallet = Box::pin(allowance_seen_by_the_wallet(&w, true)).await;
    missed(&w);
    Box::pin(crate::common::finish_listing(&mut wallet)).await;
    assert_eq!(Box::pin(asked_bob(&w, &wallet)).await, 0);
}

#[tokio::test]
async fn test_allowances_two_own_apps_answering_at_once_send_one_response() {
    // The wallet and the phone hold bob's proposal, and both reject it. The wallet's answer
    // is recorded first; the phone's write, built on the version it read, conflicts, and its
    // rejection never goes out (Paykit spec 8.3): bob gets one response.
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = Box::pin(two_wallets(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    phone.receive(None).await.unwrap();
    wallet.reject_allowance(&mine).await.unwrap();
    let error = phone.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::NotNow(_))),
        "{error:?}"
    );
    phone.receive(None).await.unwrap();
    assert!(matches!(
        phone.reject_allowance(&mine).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    let summary = shop.receive(None).await.unwrap();
    let rejections = summary
        .report
        .events
        .iter()
        .filter(|e| {
            matches!(e, AppEvent::MessageReceived { kind, .. } if kind == "paykit.allowance_rejection")
        })
        .count();
    assert_eq!(rejections, 1, "{summary:?}");
    assert!(matches!(
        shop.allowance(&theirs).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
}

/// How many messages of `kind` `app`'s next receive takes.
async fn taken(app: &mut Paykit, kind: &str) -> usize {
    let summary = app.receive(None).await.unwrap();
    summary
        .report
        .events
        .iter()
        .filter(|e| matches!(e, AppEvent::MessageReceived { kind: k, .. } if k == kind))
        .count()
}

#[tokio::test]
async fn test_allowances_restore_after_an_undelivered_proposal_proposes_once() {
    // Alice's wallet proposes while bob's inbox doesn't answer: the proposal is recorded in
    // the write that creates its record and stays in the outbox. A backup carries the outbox,
    // and the restore installs it: once flushed, bob takes that one proposal, exactly once;
    // alice's record is written once, and holds it (Paykit spec 8.3, "Recorded first").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/inbox".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    Box::pin(read_restored_records(&mut restored)).await;
    restored.flush().await.unwrap();
    let proposals = Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await
        + Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await;
    assert_eq!(proposals, 1);
    let pending = pending_key(&restored, &w.bob_id(), &sent.allowance_id);
    assert_eq!(w.record_writes(&w.alice, &pending), 1);
    assert_eq!(w.record_writes(&w.alice, &sent.record), 0);
    let views = restored.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].allowance_id, sent.allowance_id);
}

#[tokio::test]
async fn test_allowances_missed_item_hold_ends_once_the_record_shows_a_stop() {
    // After a `missed` item the phone's consent waits to be confirmed again; once the
    // record shows the Allowance ended, the entry is removed for good (Paykit spec 8.3). The
    // wallet ends it: the phone gets no message of it, and the grant's notice goes to the
    // executor alone, so the record alone ends the phone's entry.
    let w = World::new();
    let (_shop, mut wallet, _exec, mut phone, _, mine) = Box::pin(executed(&w)).await;
    missed(&w);
    phone.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "held": { "confirm_again": true } }))
    );
    wallet.end_allowance(&mine).await.unwrap();
    let got = phone.receive(None).await.unwrap();
    assert!(!received(&got, "paykit.allowance_end"), "{got:?}");
    assert!(!received(&got, "paykit.executor_notice"), "{got:?}");
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_proposal_whose_record_write_is_refused_is_never_sent() {
    // The proposal goes in the batch that creates its record: refused, nothing is sent
    // (Paykit spec 8.3, "Recorded first").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    refuse_writes(&w, 1);
    assert!(wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .is_err());
    wallet.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        0
    );
    assert!(wallet.allowances().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_answer_whose_record_write_is_refused_is_never_sent() {
    // The rejection is recorded at the version the wallet read, and sent only once that
    // write is applied: refused, nothing is sent and the Allowance stays proposed.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    refuse_writes(&w, 1);
    assert!(wallet.reject_allowance(&mine).await.is_err());
    wallet.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_rejection")).await,
        0
    );
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Proposed
    );
}

/// Bob's proposal, entered for alice's apps `to`, under `allowance_id` with a lifetime limit
/// of `limit`, so each limit gives another proposal.
fn propose_as(w: &World, to: &[[u8; 16]], allowance_id: &str, limit: &str) {
    let mut terms = terms().to_json().unwrap();
    terms["lifetime_amount_limit"] = json!(limit);
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        to,
        "paykit.allowance_proposal",
        Vec::new(),
        json!({ "allowance_id": allowance_id, "proposer_role": "allowee", "allower_generation": 1, "terms": terms })
            .as_object()
            .cloned()
            .unwrap(),
    );
}

fn competing_proposal(
    w: &World,
    shop: &Paykit,
    wallet: &Paykit,
    id: &paykit_lib::AllowanceId,
) -> StoredMessage {
    let second = signed_pending_proposal(shop, w.alice_id(), id, "allowee");
    w.alice
        .switch()
        .send_plaintext(
            w.bob_id(),
            second.cert(),
            second.plaintext(),
            &[wallet.app_id()],
        )
        .unwrap();
    second
}

#[tokio::test]
async fn test_allowances_second_proposal_conflicts_and_is_recorded() {
    // Bob proposes again under the same Allowance ID with other terms: the second proposal
    // is kept as the stop, then written into the record's `conflicted`. The Allowance shows
    // conflicted, and nothing is answered under it (Paykit spec 8.3, "Conflicted").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    let got = wallet.receive(None).await.unwrap();
    assert!(received(&got, "paykit.allowance_proposal"), "{got:?}");
    // The step wrote nothing at the switch; the later write placed the conflict.
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"cf/")
        .await
        .unwrap()
        .is_empty());
    assert!(Box::pin(pending_at(&wallet, &mine))
        .await
        .conflicted()
        .is_some());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    let error = wallet.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceConflicted)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_allowances_second_response_conflicts_and_nothing_is_sent() {
    // The shop records the first acceptance. A different signed response to the
    // same proposal then conflicts with it and prevents the shop from sending an end.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let record = Box::pin(pending_at(&shop, &theirs)).await;
    let acceptance_id = wallet
        .accept_executable_allowance(&mine, UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let applied =
        AllowanceRecord::read(&Box::pin(raw(&shop, &theirs)).await, &shop.identity()).unwrap();
    assert_eq!(applied.proposal.hash(), record.proposal().hash());
    let issued =
        AllowanceRecord::read(&Box::pin(raw(&wallet, &mine)).await, &wallet.identity()).unwrap();
    let accepted = issued.response.unwrap();
    assert_eq!(accepted.message().msg_id, acceptance_id);
    assert_eq!(applied.response.unwrap().hash(), accepted.hash());
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.allowance_rejection",
        vec![record.proposal().hash()],
        json!({ "allowance_id": record.allowance_id().as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    let error = shop.end_allowance(&theirs).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceConflicted)),
        "{error:?}"
    );
    assert_eq!(
        Box::pin(taken(&mut wallet, "paykit.allowance_end")).await,
        0
    );
}

#[tokio::test]
async fn test_allowances_second_end_is_reflected_not_conflicted() {
    // A second, different end is processed but not stored: the Allowance is ended, never
    // conflicted (Paykit spec 8.3).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accepted(&w, &mut wallet, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    let accepted = record.response.as_ref().unwrap().hash();
    let second = w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![record.proposal.hash(), accepted],
        json!({ "allowance_id": record.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    wallet.receive(None).await.unwrap();
    let held = Box::pin(value(&wallet, &mine)).await;
    assert!(held["conflicted"].is_null(), "{held}");
    assert!(held["msg_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == &json!(second)));
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
}

#[tokio::test]
async fn test_allowances_conflict_removes_the_executor_consent_for_good() {
    // The executor holds the Allowance on its consent list; bob's second proposal makes it
    // conflicted: the entry goes for good in the commit that keeps the stop, and the record
    // has no current grant.
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    let id = exec.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[exec.app_id()], id.as_str(), "0.20");
    exec.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "removed": { "reason": "conflicted" } }))
    );
    let view = exec.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Conflicted);
    assert!(!view.consented);
    assert_eq!(view.executor_app_id, None);
}

#[tokio::test]
async fn test_allowances_conflict_over_a_sealed_record_waits_for_its_key() {
    // A conflicting proposal isn't a stop that never waits: over a record the wallet holds
    // sealed it waits for the key, kept nowhere, and is taken once the record opens.
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = Box::pin(two_wallets(&w)).await;
    let (_, mine, cert) = Box::pin(sealed_allowance(&w, &mut shop, &mut wallet, &mut phone)).await;
    let id = phone.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    let got = wallet.receive(None).await.unwrap();
    assert!(!received(&got, "paykit.allowance_proposal"), "{got:?}");
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"cf/")
        .await
        .unwrap()
        .is_empty());
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
}

#[tokio::test]
async fn test_allowances_answer_over_a_sealed_record_refused_for_now() {
    // The wallet can't open the record: it may hold a response already, so nothing is
    // answered until it opens (Paykit spec 8.3).
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = Box::pin(two_wallets(&w)).await;
    let (_, mine, _) = Box::pin(sealed_allowance(&w, &mut shop, &mut wallet, &mut phone)).await;
    let error = wallet.reject_allowance(&mine).await.unwrap_err();
    assert!(is_not_now(&error), "{error:?}");
    let listed = wallet.allowances().await.unwrap_err();
    assert!(is_not_now(&listed), "{listed:?}");
}

#[tokio::test]
async fn test_allowances_crossing_rejection_and_withdrawal_leave_both_proposals_closed() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let rejection = wallet.reject_allowance(&mine).await.unwrap();
    let withdrawal = shop.end_allowance(&theirs).await.unwrap();
    assert_eq!(
        Box::pin(received_originals(&mut wallet, &[&withdrawal])).await,
        vec![withdrawal]
    );
    assert_eq!(
        Box::pin(received_originals(&mut shop, &[&rejection])).await,
        vec![rejection]
    );
    for (app, peer, public) in [(&wallet, w.bob_id(), mine), (&shop, w.alice_id(), theirs)] {
        assert!(app.app().shared_record(&public).await.unwrap().is_none());
        let key = pending_key(app, &peer, &id);
        assert!(Box::pin(held_deleted(app, &key)).await);
        assert!(matches!(
            app.allowance(&public).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowances_crossing_end_reflected_by_the_withdrawer() {
    // Bob withdraws while Alice accepts. Her matching end arrives once, and his withdrawn
    // pending proposal remains deleted.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    let id = shop.allowance(&theirs).await.unwrap().allowance_id;
    let physical = pending_key(&shop, &w.alice_id(), &id);
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    let own_end = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap()
    .end
    .unwrap()
    .message()
    .msg_id
    .clone();
    let mut received_end = 0;
    for _ in 0..3 {
        let got = shop.receive(None).await.unwrap();
        received_end += got
            .report
            .events
            .iter()
            .filter(|event| {
                matches!(event,
            AppEvent::MessageReceived { msg_id, .. } if msg_id == &own_end)
            })
            .count();
    }
    assert_eq!(received_end, 1);
    assert!(Box::pin(held_deleted(&shop, &physical)).await);
    assert!(shop
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
    assert!(shop.reject_allowance(&theirs).await.is_err());
}

#[tokio::test]
async fn test_allowances_end_naming_another_acceptance_kept_30_days_and_consent_kept() {
    // Bob's end names an acceptance other than the one the record holds: it is kept as the
    // stop 30 days, which stops payment meanwhile, and the phone's consent entry stays, since
    // the end may yet prove not to be this Allowance's. Past 30 days the stop goes, and the
    // consent counts again.
    let w = World::new();
    let (_shop, mut wallet, _exec, mut phone, _, mine) = Box::pin(executed(&w)).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id(), phone.app_id()],
        "paykit.allowance_end",
        vec![record.proposal.hash(), [8; 32]],
        json!({ "allowance_id": record.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    phone.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        phone
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "held": { "confirm_again": false } }))
    );
    assert!(!phone.allowance(&mine).await.unwrap().consented);
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    phone.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(phone
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    assert!(phone.allowance(&mine).await.unwrap().consented);
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
}

#[tokio::test]
async fn test_allowances_executor_notice_for_a_record_not_listed_adds_nothing() {
    // A notice naming a record key the consent list doesn't hold adds no entry.
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, _) = Box::pin(executed(&w)).await;
    notice(&w, &w.alice, "wallet", exec.app_id(), &[5; 32], "ended");
    exec.receive(None).await.unwrap();
    assert_eq!(Box::pin(consent_entry(&exec, &[5; 32])).await, None);
}

#[tokio::test]
async fn test_allowances_end_from_a_blocked_sender_keeps_no_stop() {
    // An end for an Allowance the wallet holds no record of is new state: from a sender the
    // user blocked, it is only counted, and no stop is kept (Paykit wire 4.1).
    let w = World::new();
    let lists = std::sync::Arc::new(crate::common::ListsStore::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new()
                .pay()
                .allowance()
                .manage_contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&lists)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let _shop = Box::pin(shop(&w)).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_allowances_kept_answer_over_a_record_that_doesnt_read_given_up_with_a_notice() {
    let w = World::new();
    let (mut phone, record, _, rejection) = Box::pin(sealed_pending_rejection(&w, true)).await;
    let writes = w.record_writes(&w.alice, &record);
    phone.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&phone, b"pr/", &rejection)).await);
    assert!(phone.notices().await.unwrap().is_empty());
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..3 {
        phone.receive(None).await.unwrap();
    }
    assert_sealed_pending(&phone, &record).await;
    assert_eq!(w.record_writes(&w.alice, &record), writes);
    assert!(!Box::pin(keeps(&phone, b"pr/", &rejection)).await);
    let notices = phone.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].msg_id.as_deref(), Some(rejection.as_str()));
}

/// A `missed` item, and Paykit's listing at the end of the next receive fails: the copy of
/// the records counts as not listed until a listing of Paykit's own.
fn missed_and_listing_fails(w: &World, wallet: &Paykit) {
    missed(w);
    fail_paykit_listing(
        &w.alice,
        crate::certificate::client_listing_requests(wallet.app().cert()),
    );
}

/// Fail Paykit's own listing at the end of the next receive of `party`'s app after a
/// `missed` item: the client lists up to its page budget before Paykit takes over.
/// A listing that fails inside the client's read fails the read.
fn fail_paykit_listing(party: &pubky_switch_client::testing::TestSigner, collections: usize) {
    // A longer native listing already holds Paykit through this read. A fault then
    // would interrupt the next native read instead of Paykit's subsequent listing.
    if collections <= pubky_switch_client::app::LISTING_PAGES_PER_READ as usize {
        party
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: "records?collection".into(),
                after: collections,
            });
    }
}

#[tokio::test]
async fn test_allowances_stop_past_30_days_kept_while_unlisted() {
    // A stop for an Allowance the wallet holds no record of is past its 30 days when the copy
    // of the records may lack records: the absence decides nothing, so it stays until a
    // listing of Paykit's own, and goes then (Paykit spec 8.1).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    missed_and_listing_fails(&w, &wallet);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(!Box::pin(keeps_end(&w, &wallet, 0)).await);
}

#[tokio::test]
async fn test_allowances_kept_answer_past_30_days_kept_while_unlisted() {
    // The same for an answer kept before its record.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let _wallet = Box::pin(wallet(&w)).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.allowance_acceptance",
        vec![[7; 32]],
        json!({ "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4" })
            .as_object()
            .cloned()
            .unwrap(),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    w.bob.switch().push_missed(1, now - 1_000, now);
    fail_paykit_listing(
        &w.bob,
        crate::certificate::client_listing_requests(shop.app().cert()),
    );
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_allowances_stored_proposal_neither_opened_nor_given_up_while_unlisted() {
    // Bob's proposal is stored; past 30 days the copy may lack records: the proposal opens
    // no record from that copy, and isn't given up, until a listing of Paykit's own.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    missed_and_listing_fails(&w, &wallet);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(wallet.notices().await.unwrap().is_empty());
    Box::pin(read_listed_records(&mut wallet)).await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(wallet.allowances().await.unwrap().len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_end_removes_the_consent_though_its_record_write_fails() {
    // The phone takes bob's end and removes its consent entry for good in the commit that
    // keeps the stop; its later write of the end into the record is refused, so the record
    // doesn't show it yet, and the entry stays removed.
    let w = World::new();
    let (mut shop, _wallet, _exec, mut phone, theirs, mine) = Box::pin(executed(&w)).await;
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    refuse_writes(&w, 1);
    phone.receive(None).await.unwrap();
    assert!(Box::pin(value(&phone, &mine)).await["end"].is_null());
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_stop_under_another_record_key_is_corrupt() {
    // A kept end read back under the key of another record than the one it names is
    // corrupt state, never taken as a stop or as none (Paykit spec 8.3).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    let (key, value) = wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .remove(0);
    let msg_id = String::from_utf8(key.clone()).unwrap();
    let msg_id = msg_id.rsplit_once('/').unwrap().1;
    let other = format!("ae/{}/{msg_id}", pubky_switch_core::enc::b64(&[3; 32]));
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[
                LocalWrite::Delete { key },
                LocalWrite::Put {
                    key: other.into_bytes(),
                    value,
                },
            ],
        )
        .await
        .unwrap();
    let error = wallet.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

#[tokio::test]
async fn test_allowances_missed_mark_other_than_true_is_corrupt() {
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    exec.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"mc".to_vec(),
                value: br#"{"v":1,"d":false}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    let error = exec.allowance(&mine).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

#[tokio::test]
async fn test_allowances_consent_under_a_key_not_its_spelling_is_corrupt() {
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, _) = Box::pin(executed(&w)).await;
    exec.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"ac/not-a-record-key".to_vec(),
                value: br#"{"v":1,"d":{"held":{"confirm_again":false}}}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    let error = exec.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

#[tokio::test]
async fn test_allowances_conflict_kept_as_the_stop_though_its_record_write_fails() {
    // The second proposal is kept as the stop before anything else: the later write of it
    // into the record is refused, and the Allowance shows conflicted all the same.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    refuse_writes(&w, 2);
    let got = wallet.receive(None).await.unwrap();
    assert!(
        Box::pin(value(&wallet, &pending_key(&wallet, &w.bob_id(), &id))).await["conflicted"]
            .is_null()
    );
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pc/")
            .await
            .unwrap()
            .len(),
        1
    );
    // The step wrote nothing at the switch, so no effect of it was refused.
    assert!(
        !got.report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::EffectRefused { .. })),
        "{got:?}"
    );
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
}

#[tokio::test]
async fn test_allowances_stop_over_a_readable_record_makes_room_when_kept() {
    // 64 ends of Allowances the wallet holds no record of, then bob's end of the one it
    // holds, naming an acceptance its record doesn't hold: that stop counts too, so keeping
    // it drops the sender's earliest (Paykit spec 8.1, 8.3).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(own_acceptance_recorded(&mut wallet, &mine)).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    w.clock.advance_ms(1);
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![record.proposal.hash(), [8; 32]],
        json!({ "allowance_id": record.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound);
    assert!(!Box::pin(keeps_end(&w, &wallet, 0)).await);
    assert!(Box::pin(keeps_end_at(&wallet, &mine)).await);
}

#[tokio::test]
async fn test_allowances_stop_over_a_record_holding_what_it_names_never_dropped_for_the_bound() {
    // Bob withdraws the Allowance the wallet holds, then ends 64 it holds no record of, in
    // one read. The withdrawal's stop is over a record that reads and holds the proposal it
    // names: it is the stop of a held Allowance, outside the bounds of the stops for records
    // not held, so the 64 later ones push none of it out, and the later write records it
    // (Paykit spec 8.3, "Ends reach the executor").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let record = Box::pin(pending_at(&wallet, &mine)).await;
    w.clock.advance_ms(1);
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![record.proposal().hash()],
        json!({ "allowance_id": record.allowance_id().as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    wallet.receive(None).await.unwrap();
    let physical = pending_key(&wallet, &w.bob_id(), record.allowance_id());
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(matches!(
        refusal(&wallet.allowance(&mine).await.unwrap_err()),
        Some(Refusal::NotFound)
    ));
    assert_eq!(Box::pin(stops(&wallet)).await, bound);
    assert!(Box::pin(keeps_end(&w, &wallet, 0)).await);
}

#[tokio::test]
async fn test_allowances_earliest_withdrawal_waits_outside_the_bound_for_its_pending_key() {
    let w = World::new();
    let (mut shop, mut writer, mut phone) = Box::pin(two_wallets(&w)).await;
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, writer.app_id(), &writer.app().cert().clone());
    writer.receive(None).await.unwrap();
    writer.receive(None).await.unwrap();
    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    writer.receive(None).await.unwrap();
    let physical = pending_key(&writer, &w.bob_id(), &sent.allowance_id);
    let canonical = writer.allowances().await.unwrap()[0].record;
    let writes = w.record_writes(&w.alice, &physical);
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(&w, phone.app_id(), &phone.app().cert().clone(), newest);
    for _ in 0..3 {
        phone.receive(None).await.unwrap();
    }
    assert_sealed_pending(&phone, &physical).await;
    let end = shop.end_allowance(&sent.record).await.unwrap();
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &phone, i);
    }
    for _ in 0..2 {
        phone.receive(None).await.unwrap();
    }
    assert!(Box::pin(keeps(&phone, b"ae/", &end)).await);
    assert_eq!(Box::pin(stops(&phone)).await, bound + 1);
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    for _ in 0..3 {
        phone.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&phone, &physical)).await);
    assert!(!Box::pin(keeps(&phone, b"ae/", &end)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(phone
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_allowances_first_rejection_kept_past_30_days_while_its_delete_is_refused() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (canonical, _, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let theirs = shop.allowances().await.unwrap()[0].record;
    let rejection = shop.reject_allowance(&theirs).await.unwrap();
    wallet.app_mut().receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &rejection)).await);
    let writes = w.record_writes(&w.alice, &physical);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    wallet.app_mut().rotate().await.unwrap();
    wallet.app_mut().flush_cursor().await.unwrap();
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &rejection)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(!Box::pin(keeps(&wallet, b"pr/", &rejection)).await);
    assert!(wallet
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
    assert!(!wallet
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.msg_id.as_deref() == Some(&rejection)));
}

#[tokio::test]
async fn test_allowances_consent_under_another_records_key_is_corrupt() {
    // A consent entry read back under the key of another Allowance's record than the one it
    // names is a corrupt store: it would let the executor pay under an Allowance its user
    // never consented to (Paykit spec 8.3).
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    let key = format!("ac/{}", pubky_switch_core::enc::b64(&mine));
    let value = exec
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let other = format!("ac/{}", pubky_switch_core::enc::b64(&[3; 32]));
    exec.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: other.into_bytes(),
                value,
            }],
        )
        .await
        .unwrap();
    let error = exec.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

#[tokio::test]
async fn test_allowances_notice_target_goes_once_its_notice_is_accepted() {
    // The wallet's end queues the executor's notice with its target (`en/`); once the
    // client reports the notice accepted, the next receive drops the target, so the targets
    // don't grow with every notice sent.
    let w = World::new();
    let (_shop, mut wallet, _exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    wallet.end_allowance(&mine).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"en/")
            .await
            .unwrap()
            .len(),
        1
    );
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"en/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_allowances_notice_target_naming_another_notice_is_corrupt() {
    // A notice target read back under another notice's `msg_id` than the one it names is a
    // corrupt store, returned by the receive, never a silent "send to no one".
    let w = World::new();
    let (_shop, mut wallet, _exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    wallet.end_allowance(&mine).await.unwrap();
    let (key, value) = wallet
        .app()
        .protocol_scan("paykit", b"en/")
        .await
        .unwrap()
        .remove(0);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[
                LocalWrite::Delete { key },
                LocalWrite::Put {
                    key: b"en/123e4567-e89b-42d3-a456-426614174000".to_vec(),
                    value,
                },
            ],
        )
        .await
        .unwrap();
    let error = wallet.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

/// `wallet` backed up and restored, its listing and reconcile done, then bob asked to send
/// again everything his inbox sent alice and the re-sent copies read: a terminal state of
/// the wallet's must come through both unchanged.
async fn restored_and_resent(w: &World, shop: &mut Paykit, mut wallet: Paykit) -> Paykit {
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.receive(None).await.unwrap();
    restored
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap();
    w.clock.advance_ms(HOUR_MS);
    shop.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    restored
}

/// An Allowance the wallet answered, ended after its acceptance when `ended`, rejected
/// otherwise; then the wallet restored from a backup and bob's messages sent to it again:
/// it stays as it ended, and nothing is answered under it (Paykit spec 8.3; wire 5.4, Seen
/// roles).
async fn terminal_through_a_restore(ended: bool) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    if ended {
        Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
        wallet.end_allowance(&mine).await.unwrap();
    } else {
        wallet.reject_allowance(&mine).await.unwrap();
        shop.receive(None).await.unwrap();
    }
    let mut restored = Box::pin(restored_and_resent(&w, &mut shop, wallet)).await;
    if ended {
        assert_eq!(
            restored.allowance(&mine).await.unwrap().status,
            AllowanceStatus::Ended
        );
    } else {
        assert!(matches!(
            refusal(&restored.allowance(&mine).await.unwrap_err()),
            Some(Refusal::NotFound)
        ));
        assert!(restored.app().shared_record(&mine).await.unwrap().is_none());
    }
    assert!(restored.reject_allowance(&mine).await.is_err());
}

#[tokio::test]
async fn test_allowances_ended_never_revived_by_a_restore_and_resends() {
    Box::pin(terminal_through_a_restore(true)).await;
}

#[tokio::test]
async fn test_allowances_rejected_never_revived_by_a_restore_and_resends() {
    Box::pin(terminal_through_a_restore(false)).await;
}

#[tokio::test]
async fn test_allowances_conflicted_kept_through_a_restore_and_resends() {
    // A conflicted Allowance, the wallet restored from a backup and bob's messages sent to it
    // again: it stays conflicted, the record keeps `conflicted`, and nothing is answered
    // (Paykit spec 8.3, "Never cleared").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(pending_at(&wallet, &mine))
        .await
        .conflicted()
        .is_some());
    let mut restored = Box::pin(restored_and_resent(&w, &mut shop, wallet)).await;
    assert_eq!(
        restored.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    assert!(Box::pin(pending_at(&restored, &mine))
        .await
        .conflicted()
        .is_some());
    let error = restored.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceConflicted)),
        "{error:?}"
    );
}

/// The permanent policy rejects a sibling's ordinary delete request before it reaches the service.
async fn permanent_delete_refused(w: &World, id: &paykit_lib::AllowanceId) {
    let mut tool = Box::pin(alice_app(
        w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let target = allowances_target(&w.bob_id(), id);
    let key = tool.app().record_key(&target).unwrap();
    tool.app_mut().read_record_fresh(&target).await.unwrap();
    let before = w.record_writes(&w.alice, &key);
    let error = tool
        .app_mut()
        .delete_record(&target, vec![])
        .await
        .unwrap_err();
    assert!(
        matches!(error, pubky_switch_client::app::AppError::Refused(pubky_switch_client::app::Refusal::Usage(ref reason)) if reason.contains("permanent policy")),
        "{error:?}"
    );
    assert_eq!(w.record_writes(&w.alice, &key), before);
}

/// A second enrolled app discards its account's pending record.
async fn delete_pending_record(w: &World, id: &paykit_lib::AllowanceId) {
    let mut tool = Box::pin(alice_app(
        w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let target = pending_target(&w.bob_id(), id);
    tool.app_mut().read_record(&target).await.unwrap();
    let op = tool
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    tool.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
}

/// Whether `app` holds its record at `record` as deleted.
async fn held_deleted(app: &Paykit, record: &[u8; 32]) -> bool {
    app.app()
        .shared_record(record)
        .await
        .unwrap()
        .is_some_and(|held| held.content == pubky_switch_client::app::RecordContent::Deleted)
}

#[tokio::test]
async fn test_allowances_restored_permanent_record_keeps_the_first_proposal() {
    // The refused delete preserves both originals across restore. A second proposal then conflicts with the first.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    Box::pin(permanent_delete_refused(&w, &id)).await;
    let mut restored = Box::pin(restored_and_resent(&w, &mut shop, wallet)).await;
    w.clock.advance_ms(HOUR_MS);
    propose_as(&w, &[restored.app_id()], id.as_str(), "0.10");
    restored.receive(None).await.unwrap();
    assert!(!Box::pin(held_deleted(&restored, &mine)).await);
    assert!(!Box::pin(value(&restored, &mine)).await["conflicted"].is_null());
    assert!(restored.reject_allowance(&mine).await.is_err());
    // The host is shown it conflicted, from the kept proposal, with nothing consented.
    let view = restored.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Conflicted);
    assert_eq!(view.counterparty, w.bob_id());
    assert!(!view.consented);
    assert!(restored
        .allowances()
        .await
        .unwrap()
        .iter()
        .any(|view| view.record == mine && view.status == AllowanceStatus::Conflicted));
}

#[tokio::test]
async fn test_allowances_second_app_retains_the_permanent_acceptance_and_conflict() {
    // Another app cannot discard the accepted Allowance. A distinct proposal still ends the phone's consent.
    let w = World::new();
    let (_shop, _wallet, _exec, mut phone, _, mine) = Box::pin(executed(&w)).await;
    let id = phone.allowance(&mine).await.unwrap().allowance_id;
    Box::pin(permanent_delete_refused(&w, &id)).await;
    phone.receive(None).await.unwrap();
    assert!(!Box::pin(held_deleted(&phone, &mine)).await);
    w.clock.advance_ms(HOUR_MS);
    propose_as(&w, &[phone.app_id()], id.as_str(), "0.10");
    phone.receive(None).await.unwrap();
    assert!(!Box::pin(held_deleted(&phone, &mine)).await);
    assert_eq!(
        phone.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "removed": { "reason": "conflicted" } }))
    );
}

#[tokio::test]
async fn test_allowances_kept_end_survives_a_restore_and_its_resent_copy() {
    // Bob ends the Allowance; the wallet keeps his end as the stop, and its write of the end
    // into the record is refused, so only the stop shows it. The wallet is then restored from
    // a backup and bob's messages sent to it again: the stop comes through, the Allowance
    // shows ended, and nothing is answered under it (Paykit spec 8.3, "Ends reach the
    // executor").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accepted(&w, &mut wallet, &mine, &mut shop, &theirs)).await;
    shop.end_allowance(&theirs).await.unwrap();
    refuse_writes(&w, 2);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(value(&wallet, &mine)).await["end"].is_null());
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"ae/")
            .await
            .unwrap()
            .len(),
        1
    );
    let mut restored = Box::pin(restored_and_resent(&w, &mut shop, wallet)).await;
    assert_eq!(
        restored.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(restored.reject_allowance(&mine).await.is_err());
}

/// An end over a record the wallet's copy doesn't hold yet, held while unlisted past the
/// client's bounded wait: the client stores the entry for the store, Paykit notes it, and
/// the end must still be taken once the records are listed.
#[tokio::test]
async fn test_allowances_end_held_past_the_bounded_wait_still_taken() {
    let w = World::new();
    let mut wallet = Box::pin(alice_app(
        &w,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
    ))
    .await;
    let me = wallet.app_id();
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut phone)).await;
    Box::pin(accepted(&w, &mut phone, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().allowance().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    let mut parked = 0;
    for _ in 0..3 {
        let s = wallet.receive(None).await.unwrap();
        assert!(!received(&s, "paykit.allowance_end"), "{s:?}");
    }
    w.clock.advance_ms(pubky_switch_core::records::DAY_MS + 1);
    for _ in 0..3 {
        let s = wallet.receive(None).await.unwrap();
        parked += s
            .report
            .events
            .iter()
            .filter(|e| matches!(e, AppEvent::Parked { .. }))
            .count();
    }
    store_bundle(&w, me, &wider);
    for _ in 0..4 {
        wallet.receive(None).await.unwrap();
    }
    let status = wallet.allowance(&mine).await.unwrap().status;
    let ends = wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .len();
    let end = Box::pin(value(&wallet, &mine)).await["end"].clone();
    assert!(
        status == AllowanceStatus::Ended,
        "status {status:?}, ae/ {ends}, end {end}, parked {parked}"
    );
}
#[tokio::test]
async fn test_allowances_attempted_permanent_delete_preserves_the_accepted_history() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, permanent) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&permanent).await.unwrap().allowance_id;
    let pending = pending_key(&wallet, &w.bob_id(), &id);
    Box::pin(accept_elsewhere(&w, &mut wallet, &permanent)).await;
    let original = Box::pin(raw(&wallet, &permanent)).await;
    let writes = w.record_writes(&w.alice, &permanent);
    Box::pin(permanent_delete_refused(&w, &id)).await;
    let target = allowances_target(&w.bob_id(), &id);
    wallet.app_mut().read_record_fresh(&target).await.unwrap();
    assert_eq!(Box::pin(raw(&wallet, &permanent)).await, original);
    assert_eq!(w.record_writes(&w.alice, &permanent), writes);
    assert!(Box::pin(held_deleted(&wallet, &pending)).await);
    assert!(wallet.reject_allowance(&permanent).await.is_err());
}

/// The `allowances` record at `target` written by `app` to `value`, `None` for a delete,
/// as a faulty own app could.
pub(super) async fn write_record(
    app: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
    value: Option<&[u8]>,
) {
    app.app_mut().read_record(target).await.unwrap();
    let op = app
        .app_mut()
        .record_op(target, value, Vec::new())
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
}

/// The target of `owner`'s record of `counterparty`'s Allowance `id`.
fn allowances_target(
    counterparty: &[u8; 32],
    id: &paykit_lib::AllowanceId,
) -> pubky_switch_client::app::RecordTarget {
    pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(counterparty, id),
    )
}

#[tokio::test]
async fn test_allowances_deferred_proposal_cannot_reopen_a_later_deleted_record() {
    // The inbox kept the original P. Its first placement must respect the sibling's
    // later creation and deletion, even after the wallet restarts.
    let w = World::new();
    let base = crate::common::store();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        base.clone(),
    ))
    .await;
    let mut sister = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    wallet.app_mut().receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &sent.msg_id)).await);
    sister.receive(None).await.unwrap();
    let active = sister.allowances().await.unwrap()[0].record;
    let proposal = Box::pin(pending_at(&sister, &active)).await;
    assert_eq!(proposal.proposal().message().msg_id, sent.msg_id);
    let physical = pending_key(&wallet, &w.bob_id(), &sent.allowance_id);
    sister.reject_allowance(&active).await.unwrap();
    assert!(Box::pin(held_deleted(&sister, &physical)).await);
    let writes = w.record_writes(&w.alice, &physical);
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = Box::pin(w.load_on_clock(&w.alice, base, app_id, w.clock.clone())).await;
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    assert!(!Box::pin(keeps(&wallet, b"pr/", &sent.msg_id)).await);
    assert!(wallet.allowances().await.unwrap().is_empty());
    assert!(wallet.app().shared_record(&active).await.unwrap().is_none());
}

#[tokio::test]
async fn test_allowances_end_survives_a_refused_permanent_delete_and_listing_pause() {
    // The current accepted record survives the delete request and receives the authenticated end after the listing resumes.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accepted(&w, &mut wallet, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    Box::pin(permanent_delete_refused(&w, &id)).await;
    wallet.receive(None).await.unwrap();
    assert!(!Box::pin(held_deleted(&wallet, &mine)).await);
    missed_and_listing_fails(&w, &wallet);
    shop.end_allowance(&theirs).await.unwrap();
    for _ in 0..4 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(wallet.end_allowance(&mine).await.is_err());
}

#[tokio::test]
async fn test_allowances_answer_over_a_deleted_record_kept_before_its_record() {
    // Bob's tool app deletes his record of the Allowance he proposed; alice's acceptance then
    // comes. Over a record held as deleted it is kept as a reply before its record, under
    // those bounds, and the record isn't written (Paykit spec 8.3, "Ending").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let pending = pending_key(&shop, &w.alice_id(), &id);
    let mut tool = w
        .join(
            &w.bob,
            "tool",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    Box::pin(write_record(
        &mut tool,
        &pending_target(&w.alice_id(), &id),
        None,
    ))
    .await;
    shop.receive(None).await.unwrap();
    assert!(Box::pin(held_deleted(&shop, &pending)).await);
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    let writes = w.record_writes(&w.bob, &pending);
    shop.receive(None).await.unwrap();
    assert!(Box::pin(held_deleted(&shop, &pending)).await);
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(w.record_writes(&w.bob, &pending), writes);
    assert!(shop.app().shared_record(&theirs).await.unwrap().is_none());
}

#[tokio::test]
async fn test_allowances_consent_paused_under_an_ignored_current_write() {
    // Another own app writes the accepted Allowance's record without its acceptance: the
    // executor-capable phone ignores that write, as it saw the acceptance, and its consent
    // no longer counts while that write is current, though its entry stays (Paykit spec 8.3,
    // "Executor").
    let w = World::new();
    let (_shop, _wallet, _exec, mut phone, _, mine) = Box::pin(executed(&w)).await;
    assert!(phone.allowance(&mine).await.unwrap().consented);
    let id = phone.allowance(&mine).await.unwrap().allowance_id;
    let mut record = Box::pin(value(&phone, &mine)).await;
    record.as_object_mut().unwrap().remove("response");
    let mut tool = Box::pin(alice_app(
        &w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    Box::pin(write_record(
        &mut tool,
        &allowances_target(&w.bob_id(), &id),
        Some(&serde_json::to_vec(&record).unwrap()),
    ))
    .await;
    phone.receive(None).await.unwrap();
    let held = phone.app().shared_record(&mine).await.unwrap().unwrap();
    assert!(held.flagged);
    assert!(!Box::pin(value(&phone, &mine)).await["response"].is_null());
    assert!(!phone.allowance(&mine).await.unwrap().consented);
    assert_eq!(
        Box::pin(consent_entry(&phone, &mine)).await,
        Some(json!({ "held": { "confirm_again": false } }))
    );
}

#[tokio::test]
async fn test_allowances_view_shows_a_blocked_counterparty() {
    // The wallet blocks bob: his Allowance shows blocked, as his requests do, and unblocked
    // once he is (Paykit spec 8.1, "Blocked counterparties").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_app(
        &w,
        "keeper",
        AppScopes::new()
            .pay()
            .allowance()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, Some(false));
    wallet.block(&w.bob_id()).await.unwrap();
    assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, Some(true));
    assert_eq!(wallet.allowances().await.unwrap()[0].blocked, Some(true));
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, Some(false));
}

#[tokio::test]
async fn test_allowances_older_backup_and_resent_proposal_keep_the_permanent_acceptance() {
    // A restore taken before the proposal recovers the original P and Q. The refused delete and a resend leave both intact.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    wallet.receive(None).await.unwrap();
    let early = wallet.backup().await.unwrap();
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let before = serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap();
    Box::pin(permanent_delete_refused(&w, &id)).await;
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &early,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.receive(None).await.unwrap();
    restored
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap();
    w.clock.advance_ms(HOUR_MS);
    shop.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(value(&restored, &mine)).await,
        serde_json::from_slice::<Value>(&before).unwrap()
    );
    let mut tool = Box::pin(alice_app(
        &w,
        "tool2",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    Box::pin(write_record(
        &mut tool,
        &allowances_target(&w.bob_id(), &id),
        Some(&before),
    ))
    .await;
    restored.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(value(&restored, &mine)).await,
        serde_json::from_slice::<Value>(&before).unwrap()
    );
    assert_eq!(
        restored.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    assert!(!restored.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_refused_permanent_delete_keeps_the_originals_and_held_conflict() {
    // A conflict retained after a refused write is still shown with its original parties and terms.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    let permanent = sent.record;
    Box::pin(accepted(&w, &mut shop, &theirs, &mut wallet, &permanent)).await;
    wallet.receive(None).await.unwrap();
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &permanent)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_rejection",
        vec![record.proposal.hash()],
        json!({ "allowance_id": record.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"cf/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(Box::pin(value(&wallet, &permanent)).await["conflicted"].is_null());
    Box::pin(permanent_delete_refused(&w, &sent.allowance_id)).await;
    wallet.receive(None).await.unwrap();
    assert!(!Box::pin(held_deleted(&wallet, &permanent)).await);
    let views = wallet.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
    let view = wallet.allowance(&permanent).await.unwrap();
    assert_eq!(views[0], view);
    assert_eq!(view.status, AllowanceStatus::Conflicted);
    assert_eq!(view.counterparty, w.bob_id());
    assert_eq!(view.allowance_id, sent.allowance_id);
    assert!(view.proposed_here);
    assert_eq!(view.role, Some(AllowanceRole::Allower));
    assert_eq!(view.terms, Some(terms()));
    assert!(!view.consented);
}

/// Alice's keeper app, holding `paykit_pay`, `paykit_allowance` and `paykit_manage_contacts`: it
/// reads the lists and blocks.
async fn keeper(w: &World) -> Paykit {
    Box::pin(alice_app(
        w,
        "keeper",
        AppScopes::new()
            .pay()
            .allowance()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await
}

/// The next read of alice's lists fails.
fn fail_list_read(w: &World) {
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/allow".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
}

/// Why sends to bob are held at the keeper (Paykit spec 8.1, "Blocked counterparties").
#[derive(Clone, Copy, Debug)]
enum SendsHeld {
    /// The keeper blocked him.
    Blocked,
    /// The copy of the deny list isn't known: a read of the lists failed at a receive, and
    /// the read the call tries first fails too.
    Unknown,
}

/// Hold sends to bob at `app` as `how` says.
async fn hold_sends(w: &World, app: &mut Paykit, how: SendsHeld) {
    match how {
        SendsHeld::Blocked => app.block(&w.bob_id()).await.unwrap(),
        SendsHeld::Unknown => {
            w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
            fail_list_read(w);
            app.receive(None).await.unwrap();
            assert_eq!(app.allowances().await.unwrap()[0].blocked, None);
            fail_list_read(w);
        }
    }
}

/// End what [`hold_sends`] held: bob unblocked, or the lists read again.
async fn release_sends(w: &World, app: &mut Paykit, how: SendsHeld) {
    if let SendsHeld::Blocked = how {
        app.unblock(&w.bob_id()).await.unwrap();
    }
}

/// The keeper's proposal to bob is refused while sends to him are held, with nothing
/// recorded, queued or received; it goes once they aren't.
async fn proposal_held(how: SendsHeld) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_app(
        &w,
        "keeper",
        AppScopes::new()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    let (_, _) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(hold_sends(&w, &mut wallet, how)).await;
    let error = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(is_not_now(&error), "{how:?} {error:?}");
    assert_eq!(wallet.allowances().await.unwrap().len(), 1);
    wallet.app_mut().flush_outbox().await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        0
    );
    Box::pin(release_sends(&w, &mut wallet, how)).await;
    wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(wallet.allowances().await.unwrap().len(), 2);
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        1
    );
}

#[tokio::test]
async fn test_allowances_proposal_to_a_blocked_counterparty_refused_as_not_now() {
    Box::pin(proposal_held(SendsHeld::Blocked)).await;
}

#[tokio::test]
async fn test_allowances_proposal_while_the_deny_copy_is_unknown_refused_as_not_now() {
    Box::pin(proposal_held(SendsHeld::Unknown)).await;
}

/// The keeper's answer to bob's proposal, `kind`, is refused while sends to him are held,
/// with the record unchanged and nothing queued or received; it goes once they aren't.
async fn answer_held(how: SendsHeld, kind: &str) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(hold_sends(&w, &mut wallet, how)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let before = Box::pin(raw(&wallet, &physical)).await;
    let status = shop.allowance(&theirs).await.unwrap().status;
    let error = Box::pin(answer(&mut wallet, kind, &mine))
        .await
        .unwrap_err();
    assert!(is_not_now(&error), "{how:?} {kind} {error:?}");
    assert_eq!(Box::pin(raw(&wallet, &physical)).await, before);
    wallet.app_mut().flush_outbox().await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, kind)).await, 0);
    assert_eq!(shop.allowance(&theirs).await.unwrap().status, status);
    Box::pin(release_sends(&w, &mut wallet, how)).await;
    Box::pin(answer(&mut wallet, kind, &mine)).await.unwrap();
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(Box::pin(taken(&mut shop, kind)).await, 1);
    assert!(matches!(
        shop.allowance(&theirs).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
}

/// `app` sends its message of `kind` for the Allowance at `record`.
async fn answer(app: &mut Paykit, kind: &str, record: &[u8; 32]) -> Result<String, PaykitError> {
    match kind {
        "paykit.allowance_rejection" => app.reject_allowance(record).await,
        "paykit.allowance_end" => app.end_allowance(record).await,
        other => panic!("no answer of kind {other}"),
    }
}

#[tokio::test]
async fn test_allowances_rejection_to_a_blocked_counterparty_refused_as_not_now() {
    Box::pin(answer_held(
        SendsHeld::Blocked,
        "paykit.allowance_rejection",
    ))
    .await;
}

#[tokio::test]
async fn test_allowances_rejection_while_the_deny_copy_is_unknown_refused_as_not_now() {
    Box::pin(answer_held(
        SendsHeld::Unknown,
        "paykit.allowance_rejection",
    ))
    .await;
}

/// Bob proposes an Allowance to alice's keeper, alice taking `role`, and she accepts it
/// ([`accepted`]); then sends to bob are held, `how`, and she ends it. A user's end is never
/// held: it stops payment, so it is recorded and sent at once, as a payee's cancellation is,
/// in either role (Paykit spec 8.1, "Blocked counterparties"; 8.3). Bob gets it once.
async fn end_goes_while_held(how: SendsHeld, role: AllowanceRole) {
    let w = World::new();
    let mut shop = match role {
        AllowanceRole::Allower => Box::pin(shop(&w)).await,
        _ => {
            let mut app = w
                .join(
                    &w.bob,
                    "shop",
                    AppScopes::new()
                        .allowance_execute()
                        .request()
                        .build()
                        .unwrap(),
                )
                .await;
            app.app_mut().invite(&w.alice_id()).await.unwrap();
            Box::pin(crate::common::finish_listing(&mut app)).await;
            app
        }
    };
    let mut wallet = Box::pin(keeper(&w)).await;
    let bobs = match role {
        AllowanceRole::Allower => AllowanceRole::Allowee,
        _ => AllowanceRole::Allower,
    };
    let theirs = propose_in_role(&mut shop, w.alice_id(), bobs)
        .await
        .unwrap()
        .record;
    wallet.receive(None).await.unwrap();
    let mine = wallet.allowances().await.unwrap()[0].record;
    assert_eq!(wallet.allowance(&mine).await.unwrap().role, Some(role));
    Box::pin(accepted(&w, &mut wallet, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    match how {
        SendsHeld::Blocked => wallet.block(&w.bob_id()).await.unwrap(),
        SendsHeld::Unknown => {
            w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
            fail_list_read(&w);
            wallet.receive(None).await.unwrap();
            assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, None);
        }
    }
    wallet.end_allowance(&mine).await.unwrap();
    assert!(
        !Box::pin(value(&wallet, &mine)).await["end"].is_null(),
        "{how:?}"
    );
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 1);
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Ended
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

#[tokio::test]
async fn test_allowances_end_to_a_blocked_counterparty_recorded_and_sent_at_once_as_allower() {
    Box::pin(end_goes_while_held(
        SendsHeld::Blocked,
        AllowanceRole::Allower,
    ))
    .await;
}

#[tokio::test]
async fn test_allowances_end_to_a_blocked_counterparty_recorded_and_sent_at_once_as_allowee() {
    Box::pin(end_goes_while_held(
        SendsHeld::Blocked,
        AllowanceRole::Allowee,
    ))
    .await;
}

#[tokio::test]
async fn test_allowances_end_while_the_deny_copy_is_unknown_recorded_and_sent_at_once_as_allower() {
    Box::pin(end_goes_while_held(
        SendsHeld::Unknown,
        AllowanceRole::Allower,
    ))
    .await;
}

#[tokio::test]
async fn test_allowances_end_while_the_deny_copy_is_unknown_recorded_and_sent_at_once_as_allowee() {
    Box::pin(end_goes_while_held(
        SendsHeld::Unknown,
        AllowanceRole::Allowee,
    ))
    .await;
}

/// Bob's withdrawal crosses the keeper's acceptance, and sends to him are held, `how`, when
/// the keeper reads it. Returns the record keys on bob's side and on alice's.
async fn crossing_while_held(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    how: SendsHeld,
) -> ([u8; 32], [u8; 32]) {
    let (theirs, mine) = Box::pin(proposed(w, shop, wallet)).await;
    Box::pin(accept_elsewhere(w, wallet, &mine)).await;
    shop.end_allowance(&theirs).await.unwrap();
    match how {
        SendsHeld::Blocked => wallet.block(&w.bob_id()).await.unwrap(),
        SendsHeld::Unknown => {
            w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
            fail_list_read(w);
        }
    }
    wallet.receive(None).await.unwrap();
    (theirs, mine)
}

/// Whether `app`'s record at `record` holds an end naming the proposal and the acceptance.
async fn ends_naming_both(app: &Paykit, record: &[u8; 32]) -> bool {
    let held = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(app, record)).await).unwrap(),
        &app.identity(),
    )
    .unwrap();
    let accepted = held.response.as_ref().map(|r| r.hash());
    held.end.is_some_and(|end| {
        end.sender() == app.identity()
            && end.message().refs == vec![held.proposal.hash(), accepted.unwrap()]
    })
}

/// Bob's withdrawal crosses the keeper's acceptance while sends to him are held: the keeper
/// makes its own end and records it in the Allowance record at once, naming the proposal and
/// the acceptance, so every own app finds the Allowance ended; only the end's message to bob
/// waits, owed, and goes from the record, once, at the first receive after sends aren't
/// held (Paykit spec 8.1, "Blocked counterparties"; 8.3, "Ending"; Paykit wire 4.16).
async fn crossing_end_held(how: SendsHeld) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(crossing_while_held(&w, &mut shop, &mut wallet, how)).await;
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(Box::pin(ends_naming_both(&wallet, &mine)).await, "{how:?}");
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"eo/")
            .await
            .unwrap()
            .len(),
        1
    );
    wallet.app_mut().flush_outbox().await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    if let SendsHeld::Blocked = how {
        // Still held at a later receive while bob is blocked.
        wallet.receive(None).await.unwrap();
        assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    }
    Box::pin(release_sends(&w, &mut wallet, how)).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

#[tokio::test]
async fn test_allowances_crossing_end_to_a_blocked_counterparty_recorded_at_once_sent_once_unblocked(
) {
    Box::pin(crossing_end_held(SendsHeld::Blocked)).await;
}

#[tokio::test]
async fn test_allowances_crossing_end_while_the_deny_copy_is_unknown_recorded_at_once_sent_once_it_reads(
) {
    Box::pin(crossing_end_held(SendsHeld::Unknown)).await;
}

#[tokio::test]
async fn test_allowances_restore_keeps_the_recorded_crossing_end() {
    // The record can establish the signed answer. Releasing a hold cannot settle its delivery.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (theirs, mine) = Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut wallet,
        SendsHeld::Blocked,
    ))
    .await;
    assert!(Box::pin(ends_naming_both(&wallet, &mine)).await);
    let backup = wallet.backup().await.unwrap();
    assert!(matches!(
        shop.allowance(&theirs).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    drop(wallet);
    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    Box::pin(read_restored_records(&mut restored)).await;
    assert!(restored
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
    assert!(restored
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    restored.unblock(&w.bob_id()).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    restored.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    assert!(matches!(
        shop.allowance(&theirs).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
}

#[tokio::test]
async fn test_allowances_crossing_end_after_an_unanswered_write_reports_its_unknown_delivery() {
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    // The withdrawal reaches Alice before the block, and she processes it afterwards.
    shop.end_allowance(&theirs).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    for _ in 0..4 {
        w.alice.switch().fail_next(Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    }
    wallet.receive(None).await.unwrap();
    let markers = wallet.app().protocol_scan("paykit", b"eb/").await.unwrap();
    assert_eq!(
        markers.len(),
        1,
        "expected the unpaid crossing end in an unresolved batch"
    );
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"end/")
            .await
            .unwrap()
            .len(),
        1
    );
    let backup = wallet.backup().await.unwrap();
    // The original installation settles the same write once the connection returns.
    // Its backup still dates from before that response, while the counterparty stays blocked.
    for _ in 0..6 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(ends_naming_both(&wallet, &mine)).await);
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"eb/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    let end = Box::pin(signed_recorded_end(&wallet, &mine)).await;
    let origin_key = format!("end/{}", end.message().msg_id);
    drop(wallet);

    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    assert!(restored
        .app()
        .protocol_get("paykit", origin_key.as_bytes())
        .await
        .unwrap()
        .is_some());
    Box::pin(read_restored_records(&mut restored)).await;
    assert!(Box::pin(ends_naming_both(&restored, &mine)).await);
    assert!(restored
        .app()
        .protocol_scan("paykit", b"eb/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    restored.unblock(&w.bob_id()).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    assert!(restored.notices().await.unwrap().iter().any(|notice| {
        notice.kind == paykit_sdk::NoticeKind::PossiblyDelivered
            && notice.counterparty == Some(w.bob_id())
            && notice.msg_id.as_ref() == Some(&end.message().msg_id)
            && notice.message_hash == Some(end.hash())
    }));
    restored.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

#[tokio::test]
async fn test_allowances_view_blocked_unknown_while_the_deny_copy_is_unknown() {
    // A failed read of the lists leaves the copy unknown: the view says so, never "not
    // blocked"; the next read shows him not blocked (Paykit spec 8.1).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, Some(false));
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    fail_list_read(&w);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, None);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.allowance(&mine).await.unwrap().blocked, Some(false));
}

#[tokio::test]
async fn test_allowances_answer_dropped_for_the_bound_taken_again_from_a_resend_and_placed() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (proposal, id, rejected) = Box::pin(rejection_before_its_pending(
        &w,
        &mut shop,
        &mut wallet,
        AllowanceRole::Allowee,
    ))
    .await;
    assert!(shop.allowances().await.unwrap().is_empty());
    w.clock.advance_ms(1);
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.allowance_rejection",
            vec![[u8::try_from(i).unwrap(); 32]],
            json!({ "allowance_id": paykit_lib::AllowanceId::generate().unwrap().as_str() })
                .as_object()
                .cloned()
                .unwrap(),
        );
    }
    shop.app_mut().receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        bound
    );
    let theirs = Box::pin(records_proposal(&mut shop, &proposal, &id)).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Proposed
    );
    wallet.receive(None).await.unwrap();
    Box::pin(receives_resent_rejection(&mut shop, &theirs, &rejected)).await;
}

/// Bob's shop proposes an Allowance to alice, and `wallet` answers it with `kind`, a
/// rejection or, after an app of another implementation accepted, an end: the answer's batch
/// is applied with its answer lost, so the message is in the wallet's record and in no
/// outbox, and the wallet is backed up then. Returns the record keys on alice's side and on bob's, and the backup.
async fn backed_up_with_its_answer_unqueued(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    kind: &str,
) -> ([u8; 32], [u8; 32], paykit_sdk::PaykitBackup, Option<String>) {
    let (theirs, mine) = Box::pin(proposed(w, shop, wallet)).await;
    if kind == "paykit.allowance_end" {
        Box::pin(accept_elsewhere(w, wallet, &mine)).await;
        shop.receive(None).await.unwrap();
    }
    // The switch applies the batch and its answer is lost, so the batch stays kept: the
    // client sends it again only at a later call.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let answered = Box::pin(answer(wallet, kind, &mine)).await;
    assert_eq!(Box::pin(taken(shop, kind)).await, 0, "{answered:?}");
    (mine, theirs, wallet.backup().await.unwrap(), answered.ok())
}

/// Finish the client's owed listing and Paykit's listing under the certificate now held.
async fn read_listed_records(wallet: &mut Paykit) {
    Box::pin(read_listed_records_while(wallet, || {})).await;
}

async fn read_listed_records_while(wallet: &mut Paykit, before_read: impl Fn()) {
    for _ in 0..4 {
        before_read();
        let summary = wallet.receive(None).await.unwrap();
        if summary.records_unlisted.is_some() || wallet.app().listing_owed().await.unwrap() {
            continue;
        }
        match wallet.allowances().await {
            Ok(_) => return,
            Err(error) => assert!(
                matches!(error.refusal(), Some(Refusal::NotNow(_))),
                "{error:?}"
            ),
        }
    }
    panic!("the wallet's Allowance records are still unlisted");
}

async fn read_restored_records(wallet: &mut Paykit) {
    Box::pin(read_listed_records(wallet)).await;
    assert!(wallet.app().restore_held_cursor().is_some());
}

/// The user's block is already durable when the first page has not finished the restore.
async fn block_during_restore(wallet: &mut Paykit, peer: &[u8; 32]) {
    let result = wallet.block(peer).await;
    if let Err(error) = result {
        assert!(
            matches!(
                error,
                paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Refused(
                    pubky_switch_client::app::Refusal::NotNow(_)
                ))
            ),
            "{error:?}"
        );
        let mark = format!("bk/{}", pubky_switch_core::enc::b64(peer));
        assert!(wallet
            .app()
            .protocol_get("paykit", mark.as_bytes())
            .await
            .unwrap()
            .is_some());
        Box::pin(read_restored_records(wallet)).await;
        wallet.block(peer).await.unwrap();
    }
}

/// Restore `backup` onto a new store as an app of alice's, without a receive.
async fn restored_unreceived(w: &World, backup: &paykit_sdk::PaykitBackup) -> Paykit {
    Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap()
}

async fn queued_rejection(w: &World, wallet: &mut Paykit, canonical: &[u8; 32]) -> String {
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/inbox".into(),
            after: 0,
        });
    let original = wallet.reject_allowance(canonical).await.unwrap();
    assert!(matches!(
        wallet.app().delivery(&original).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Queued { .. })
    ));
    original
}

/// A freshly restored wallet retains an uncertain Allowance answer for reconciliation.
async fn restored_answer(kind: &str) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (_, theirs, backup, answered) = Box::pin(backed_up_with_its_answer_unqueued(
        &w,
        &mut wallet,
        &mut shop,
        kind,
    ))
    .await;
    let previous = shop.allowance(&theirs).await.unwrap().status;
    drop(wallet);
    let mut wallet = Box::pin(restored_unreceived(&w, &backup)).await;
    Box::pin(read_restored_records(&mut wallet)).await;
    assert!(wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    if kind == "paykit.allowance_rejection" {
        let original = answered.expect("the exact prepared rejection");
        assert_eq!(
            Box::pin(received_originals(&mut shop, &[&original])).await,
            [original]
        );
        assert!(matches!(
            shop.allowance(&theirs).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
    } else {
        assert_eq!(Box::pin(taken(&mut shop, kind)).await, 0);
        assert_eq!(shop.allowance(&theirs).await.unwrap().status, previous);
        assert!(wallet
            .notices()
            .await
            .unwrap()
            .iter()
            .any(|notice| notice.kind == paykit_sdk::NoticeKind::PossiblyDelivered));
    }
    let before = w.bob.switch().entries_from(&w.alice_id());
    wallet.receive(None).await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), before);
}

#[tokio::test]
async fn test_allowances_restore_delivers_the_rejection_after_its_exact_delete_is_confirmed() {
    Box::pin(restored_answer("paykit.allowance_rejection")).await;
}

#[tokio::test]
async fn test_allowances_restore_reports_an_unanswered_end() {
    Box::pin(restored_answer("paykit.allowance_end")).await;
}

/// An applied original on restore waits for the current deny state to permit sending.
async fn restored_rejection_waits_for_permission(how: SendsHeld) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let kind = "paykit.allowance_rejection";
    let (mine, theirs, backup, answered) = Box::pin(backed_up_with_its_answer_unqueued(
        &w,
        &mut wallet,
        &mut shop,
        kind,
    ))
    .await;
    let previous = shop.allowance(&theirs).await.unwrap().status;
    drop(wallet);
    let mut wallet = Box::pin(restored_unreceived(&w, &backup)).await;
    if let SendsHeld::Blocked = how {
        Box::pin(block_during_restore(&mut wallet, &w.bob_id())).await;
    }
    Box::pin(read_listed_records_while(&mut wallet, || {
        if let SendsHeld::Unknown = how {
            fail_list_read(&w);
        }
    }))
    .await;
    let original = answered.expect("the exact prepared rejection");
    assert!(matches!(
        wallet.allowance(&mine).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    assert!(
        Box::pin(received_originals(&mut shop, &[&original]))
            .await
            .is_empty(),
        "{how:?}"
    );
    assert_eq!(shop.allowance(&theirs).await.unwrap().status, previous);
    if let SendsHeld::Blocked = how {
        wallet.unblock(&w.bob_id()).await.unwrap();
    }
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    Box::pin(read_listed_records(&mut wallet)).await;
    wallet.flush().await.unwrap();
    assert_eq!(
        Box::pin(received_originals(&mut shop, &[&original])).await,
        [original.as_str()]
    );
    wallet.flush().await.unwrap();
    assert!(Box::pin(received_originals(&mut shop, &[&original]))
        .await
        .is_empty());
}

#[tokio::test]
async fn test_allowances_restore_own_rejection_to_a_blocked_counterparty_waits_until_unblocked() {
    Box::pin(restored_rejection_waits_for_permission(SendsHeld::Blocked)).await;
}

#[tokio::test]
async fn test_allowances_restore_own_rejection_waits_while_the_deny_list_is_unknown() {
    Box::pin(restored_rejection_waits_for_permission(SendsHeld::Unknown)).await;
}

#[tokio::test]
async fn test_allowances_restore_own_rejection_in_a_record_sealed_under_a_key_not_held_waits_for_it(
) {
    // The restored wallet cannot read the newly recorded P until it has the key for
    // its epoch. Its queued rejection of the older P still needs both current records.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let wallet_app = wallet.app_id();
    let cert = wallet.app().cert().clone();
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let wallet_pending = pending_key(&wallet, &w.bob_id(), &id);
    let shop_pending = pending_key(&shop, &w.alice_id(), &id);
    let rejection = Box::pin(queued_rejection(&w, &mut wallet, &mine)).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let next = signed_pending_proposal(&shop, w.alice_id(), &id, "allowee");
    let recorded = paykit_lib::records::PendingAllowanceRecord::new(&w.alice_id(), next)
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(
        &mut phone,
        &pending_target(&w.bob_id(), &id),
        Some(&recorded),
    ))
    .await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(&w, wallet_app, &cert, newest);
    let mut wallet = Box::pin(restored_unreceived(&w, &backup)).await;
    let kind = "paykit.allowance_rejection";
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    let sealed = wallet
        .app()
        .shared_record(&wallet_pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            sealed.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::KeyNotHeld { .. }
            )
        ),
        "{:?}",
        sealed.content
    );
    assert_eq!(Box::pin(taken(&mut shop, kind)).await, 0);
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .iter()
        .any(|m| m.msg_id == rejection));
    crate::unrequested::store_bundle(&w, wallet_app, &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    w.clock.advance_ms(60_000);
    wallet.app_mut().flush_message(&rejection).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, kind)).await, 1);
    assert!(Box::pin(held_deleted(&shop, &shop_pending)).await);
    assert_eq!(Box::pin(raw(&wallet, &wallet_pending)).await, recorded);
}

/// How many messages `app` takes as duplicates at one receive, besides those of `kind` it
/// takes as new: a message queued again that its recipient already took arrives as one.
async fn taken_or_duplicate(app: &mut Paykit, kind: &str) -> usize {
    let summary = app.receive(None).await.unwrap();
    summary
        .report
        .events
        .iter()
        .filter(|e| match e {
            AppEvent::MessageReceived { kind: k, .. } => k == kind,
            AppEvent::Duplicate { .. } => true,
            _ => false,
        })
        .count()
}

/// An older backup carries the undelivered rejection of a previous proposal.
/// A different signed proposal now occupies the same pending key.
async fn restored_rejection_over_conflict(w: &World, shop: &mut Paykit, shared: bool) {
    let mut wallet = Box::pin(wallet(w)).await;
    let mut phone = Box::pin(alice_app(
        w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let (_, key) = Box::pin(proposed(w, shop, &mut wallet)).await;
    let id = wallet.allowance(&key).await.unwrap().allowance_id;
    let original = Box::pin(queued_rejection(w, &mut wallet, &key)).await;
    let backup = wallet.backup().await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let p = signed_pending_proposal(shop, w.alice_id(), &id, "allowee");
    phone.receive(None).await.unwrap();
    assert_eq!(Box::pin(records_proposal(&mut phone, &p, &id)).await, key);
    let mut restored = Box::pin(restored_unreceived(w, &backup)).await;
    Box::pin(read_listed_records(&mut restored)).await;
    assert_eq!(
        Box::pin(pending_at(&restored, &key))
            .await
            .proposal()
            .hash(),
        p.hash()
    );
    propose_as(w, &[app_id], id.as_str(), "0.20");
    if !shared {
        refuse_writes(w, 12);
    }
    let got = restored.receive(None).await.unwrap();
    assert!(received(&got, "paykit.allowance_proposal"), "{got:?}");
    for _ in 0..2 {
        restored.receive(None).await.unwrap();
    }
    let physical = pending_key(&restored, &w.bob_id(), &id);
    let view = restored.allowance(&key).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Conflicted);
    assert!(Box::pin(keeps(&restored, b"pc/", &b64_of(&physical))).await);
    let held = restored
        .app()
        .shared_record(&physical)
        .await
        .unwrap()
        .unwrap();
    let current = paykit_lib::records::PendingAllowanceRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    assert_eq!(current.proposal().hash(), p.hash());
    assert_eq!(current.conflicted().is_some(), shared);
    w.clock.advance_ms(60_000);
    restored.app_mut().flush_message(&original).await.unwrap();
    assert!(!matches!(
        restored.app().delivery(&original).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    assert_eq!(Box::pin(taken(shop, "paykit.allowance_rejection")).await, 0);
}

#[tokio::test]
async fn test_allowances_restore_sends_nothing_under_a_kept_conflict() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    Box::pin(restored_rejection_over_conflict(&w, &mut shop, false)).await;
}

#[tokio::test]
async fn test_allowances_restore_sends_nothing_for_a_conflicted_record() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    Box::pin(restored_rejection_over_conflict(&w, &mut shop, true)).await;
}

#[tokio::test]
async fn test_allowances_restore_sends_no_proposal_after_the_counterpartys_answer() {
    // The wallet's proposal goes in a batch whose answer is lost, and the wallet is backed
    // up then. The batch goes at its next read, bob rejects, and the record holds his
    // rejection. Restored from the early backup, the wallet doesn't queue its proposal
    // again: bob has answered it (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let backup = wallet.backup().await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.app_mut().flush_outbox().await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        1,
        "{sent:?}"
    );
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.reject_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    let physical = pending_key(&wallet, &w.bob_id(), &sent.allowance_id);
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert!(wallet
        .app()
        .shared_record(&sent.record)
        .await
        .unwrap()
        .is_none());
    drop(wallet);
    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    Box::pin(read_restored_records(&mut restored)).await;
    assert!(Box::pin(held_deleted(&restored, &physical)).await);
    assert!(restored
        .app()
        .shared_record(&sent.record)
        .await
        .unwrap()
        .is_none());
    assert!(restored
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        Box::pin(taken_or_duplicate(&mut shop, "paykit.allowance_proposal")).await,
        0
    );
}

#[tokio::test]
async fn test_allowances_end_dropped_for_the_bound_taken_again_from_a_resend() {
    // Bob's shop sends an end of an Allowance the wallet holds no record of; his 64 later ends
    // of others push it out of the bound. Its pair is forgotten in the commit that drops it,
    // and it counts in bob's mark of dropped replies, due at once: an ended Allowance's record
    // may come at any time after its end was dropped. The mark asks him to re-send by itself,
    // once; the same end is taken as a new message and kept as the stop again (Paykit spec
    // 8.1, "Replies before their record"; 8.3, "Ends reach the executor"; Switch spec 11).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    // The proposal never reached Alice. Bob withdraws that queued delivery and ends
    // the Allowance through Paykit, which retains the signed end's producer origin.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/inbox".into(),
            after: 0,
        });
    let proposal = Box::pin(shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms()))
        .await
        .unwrap();
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &proposal.msg_id)
        .await
        .unwrap());
    let first = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &proposal.allowance_id))
        .unwrap();
    Box::pin(shop.end_allowance(&proposal.record))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps_end_at(&wallet, &first)).await);
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    // Asked at once, in the receive that drops it, with no record of bob's held, and once
    // only.
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert!(!Box::pin(keeps_end_at(&wallet, &first)).await);
    assert_eq!(Box::pin(stops(&wallet)).await, bound);
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pd/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    shop.receive(None).await.unwrap();
    let got = wallet.receive(None).await.unwrap();
    assert!(received(&got, "paykit.allowance_end"), "{got:?}");
    assert!(Box::pin(keeps_end_at(&wallet, &first)).await);
}

#[tokio::test]
async fn test_allowances_proposal_over_a_deleted_record_from_a_blocked_sender_keeps_no_stop() {
    // Another own app deletes the record of bob's Allowance; the keeper then blocks bob, and
    // a proposal of his under the same Allowance ID comes. Over a record held as deleted it
    // is new state, which the spam rule gates: it is only counted, and no conflict's stop is
    // kept (Paykit wire 4.1; Paykit spec 8.3, "Ending").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    Box::pin(delete_pending_record(&w, &id)).await;
    wallet.receive(None).await.unwrap();
    let pending = pending_key(&wallet, &w.bob_id(), &id);
    assert!(Box::pin(held_deleted(&wallet, &pending)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    wallet.block(&w.bob_id()).await.unwrap();
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"cf/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
}

/// The original signed rejection is received once on a resend.
async fn receives_resent_rejection(app: &mut Paykit, canonical: &[u8; 32], original: &str) {
    let view = app.allowance(canonical).await.unwrap();
    let pending = pending_key(app, &view.counterparty, &view.allowance_id);
    let read = app.receive(None).await.unwrap();
    assert_eq!(read.resends, 0);
    assert!(read.report.events.iter().any(|e| matches!(e,
        AppEvent::MessageReceived { msg_id, .. } if msg_id == original)));
    assert!(Box::pin(held_deleted(app, &pending)).await);
    assert!(app.app().shared_record(canonical).await.unwrap().is_none());
}

/// After an old backup is restored, the proposer receives the original signed rejection
/// while its service shows no live proposal.
async fn rejection_before_its_pending(
    w: &World,
    proposer: &mut Paykit,
    responder: &mut Paykit,
    role: AllowanceRole,
) -> (StoredMessage, paykit_lib::AllowanceId, String) {
    use pubky_switch_client::app::{KeptReason, RecordRead};
    use pubky_switch_client::testing::Fault;

    let backup = proposer.backup().await.unwrap();
    let (own, peer) = if proposer.identity() == w.alice_id() {
        (&w.alice, &w.bob)
    } else {
        (&w.bob, &w.alice)
    };
    let sent = propose_in_role(proposer, responder.identity(), role)
        .await
        .unwrap();
    let target = pending_target(&responder.identity(), &sent.allowance_id);
    let physical = proposer.app().record_key(&target).unwrap();
    let proposal = Box::pin(pending_at(proposer, &sent.record))
        .await
        .proposal()
        .clone();
    responder.receive(None).await.unwrap();
    let theirs = responder.allowances().await.unwrap()[0].record;
    own.switch().fail_next(Fault::Unreachable {
        path: "/inbox".into(),
        after: 0,
    });
    let rejected = responder.reject_allowance(&theirs).await.unwrap();
    assert!(matches!(
        responder.app().delivery(&rejected).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Queued { .. })
    ));

    peer.switch().fail_next(Fault::Unreachable {
        path: "/inbox".into(),
        after: 0,
    });
    let end = proposer.end_allowance(&sent.record).await.unwrap();
    assert!(matches!(
        proposer.app().delivery(&end).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Queued { .. })
    ));
    proposer.app_mut().receive(None).await.unwrap();
    proposer.app_mut().flush_cursor().await.unwrap();
    *proposer = Box::pin(Paykit::restore(
        w.env(own, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    let first_read = proposer.receive(None).await.unwrap();
    let observed = proposer.app_mut().read_record_fresh(&target).await.unwrap();
    assert!(
        match &observed {
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: None,
            } => true,
            RecordRead::Fresh(r)
            | RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: Some(r),
            } =>
                r.key == physical
                    && r.authenticated
                    && !r.flagged
                    && !r.doubtful
                    && !r.not_taken
                    && !r.divergent
                    && !r.different_write_at_held_version
                    && r.content == pubky_switch_client::app::RecordContent::Deleted,
            _ => false,
        },
        "current proposal after restore: {observed:?}"
    );

    w.clock.advance_ms(60_000);
    let delivery_state = responder.app_mut().flush_message(&rejected).await.unwrap();
    let mut observed = vec![first_read];
    for _ in 0..3 {
        observed.push(proposer.receive(None).await.unwrap());
    }
    let accepted = observed
        .iter()
        .flat_map(|read| &read.report.events)
        .filter(|e| {
            matches!(e,
        AppEvent::MessageReceived { msg_id, .. } if *msg_id == rejected)
        })
        .count();
    assert_eq!(
        accepted,
        1,
        "original {rejected} delivery {delivery_state:?}, recipient tip {}, reads {observed:?}",
        own.switch().tip_seq()
    );
    assert_eq!(Box::pin(kept_under(proposer, b"pr/")).await, 1);
    (proposal, sent.allowance_id, rejected)
}

/// Alice's `wallet`, a payer app, sends a proposal to bob as allower alone, without writing
/// its record, as another of alice's apps would before this one holds the record; bob's shop
/// rejects it, and the wallet keeps the rejection before its record. Bob's 64 later
/// rejections of proposals alice never sent drop it for the bound, its pair forgotten, and
/// it counts in bob's mark. `before` runs then; then the wallet's record of the Allowance is
/// written, as the other app's batch would write it. Returns the record key on alice's side
/// and the rejection's `msg_id`.
async fn wallet_answer_dropped_then_recorded(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    before: impl AsyncFnOnce(&World, &mut Paykit, &mut Paykit),
) -> ([u8; 32], String) {
    let (proposal, id, rejected) = Box::pin(rejection_before_its_pending(
        w,
        wallet,
        shop,
        AllowanceRole::Allower,
    ))
    .await;
    w.clock.advance_ms(1);
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.inject(
            &w.bob,
            "shop",
            &w.alice,
            &[wallet.app_id()],
            "paykit.allowance_rejection",
            vec![[u8::try_from(i).unwrap(); 32]],
            json!({ "allowance_id": paykit_lib::AllowanceId::generate().unwrap().as_str() })
                .as_object()
                .cloned()
                .unwrap(),
        );
    }
    wallet.app_mut().receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        bound
    );
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pd/")
            .await
            .unwrap()
            .len(),
        1
    );
    before(w, wallet, shop).await;
    let mine = Box::pin(records_proposal(wallet, &proposal, &id)).await;
    (mine, rejected)
}

#[tokio::test]
async fn test_allowances_answer_dropped_at_a_payer_app_proposer_asked_again_once_and_placed() {
    // The other side of the dropped-answer test: alice's wallet, holding `paykit_pay` and not
    // `paykit_request`, proposed as allower. Once it holds the Allowance record, bob's mark is due
    // from that record alone and asks him once; the same rejection comes again, is taken as
    // never processed, and is placed (Paykit spec 8.1, "Replies before their record").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, rejected) = Box::pin(wallet_answer_dropped_then_recorded(
        &w,
        &mut wallet,
        &mut shop,
        async |_, wallet, _| {
            assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
        },
    ))
    .await;
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Proposed
    );
    shop.receive(None).await.unwrap();
    Box::pin(receives_resent_rejection(&mut wallet, &mine, &rejected)).await;
}

#[tokio::test]
async fn test_allowances_answer_dropped_at_a_payer_app_asked_again_only_once_unblocked() {
    // As above at the keeper, which reads the lists: bob is blocked when the record arrives,
    // so the due mark asks him nothing; once he is unblocked it asks him, once (Paykit spec
    // 8.1, "Blocked counterparties": no re-send request goes to a blocked counterparty).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_app(
        &w,
        "keeper",
        AppScopes::new()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    let (mine, rejected) = Box::pin(wallet_answer_dropped_then_recorded(
        &w,
        &mut wallet,
        &mut shop,
        async |_, _, _| {},
    ))
    .await;
    wallet.block(&w.bob_id()).await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    shop.receive(None).await.unwrap();
    Box::pin(receives_resent_rejection(&mut wallet, &mine, &rejected)).await;
}

#[tokio::test]
async fn test_allowances_owed_crossing_end_not_sent_once_the_allowance_is_conflicted() {
    // The keeper's crossing end is recorded while bob is blocked, its message owed; then a
    // second proposal under the same Allowance ID conflicts it. Nothing is sent for a
    // conflicted Allowance, as a restore's re-send sends nothing for one: once bob is
    // unblocked the owed mark goes with nothing sent (Paykit spec 8.3, "Conflicted").
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut wallet,
        SendsHeld::Blocked,
    ))
    .await;
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"eo/")
            .await
            .unwrap()
            .len(),
        1
    );
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

#[tokio::test]
async fn test_allowances_owed_crossing_end_survives_a_refused_permanent_delete() {
    // The original crossing end stays owed while its recipient is blocked and goes once after unblocking.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut wallet,
        SendsHeld::Blocked,
    ))
    .await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    Box::pin(permanent_delete_refused(&w, &id)).await;
    wallet.receive(None).await.unwrap();
    assert!(!Box::pin(held_deleted(&wallet, &mine)).await);
    assert!(!wallet
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 1);
}

#[tokio::test]
async fn test_allowances_owed_crossing_end_kept_through_a_store_failure_and_sent_once() {
    // Bob is unblocked, and the store fails the commit that would queue the keeper's owed
    // crossing end: the receive returns that failure, the mark stays, and the next receive
    // sends the end, once.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let fault = std::sync::Arc::new(pubky_switch_client::store::FaultStore::new(
        crate::common::store(),
        pubky_switch_client::store::Fault::None,
    ));
    let mut wallet = w
        .join_with(
            &w.alice,
            "keeper",
            AppScopes::new()
                .pay()
                .allowance()
                .manage_contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&fault)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut wallet,
        SendsHeld::Blocked,
    ))
    .await;
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"eo/")
            .await
            .unwrap()
            .len(),
        1
    );
    wallet.unblock(&w.bob_id()).await.unwrap();
    fault.fail_put_once(b"out/");
    assert!(wallet.receive(None).await.is_err());
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"eo/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 1);
}

#[tokio::test]
async fn test_allowances_dropped_answer_asked_before_payment_and_allowance_records() {
    // The missing answer is asked for before either record arrives. The payment and
    // Allowance record each leave that single ask in effect.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, rejected) = Box::pin(wallet_answer_dropped_then_recorded(
        &w,
        &mut wallet,
        &mut shop,
        async |w, wallet, shop| {
            assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
            w.list_to_alice(shop, &[wallet.app_id()], list_version(1, 1))
                .await;
            wallet.receive(None).await.unwrap();
            for _ in 0..4 {
                match wallet.reconcile().await {
                    Ok(_) => break,
                    Err(error) => assert!(matches!(error.refusal(),Some(Refusal::NotNow(reason)) if reason.contains("continue the listing")), "{error:?}"),
                }
            }
            for _ in 0..4 {
                match wallet.end_reconcile(paykit_sdk::UserConfirmed::new()).await {
                    Ok(()) => break,
                    Err(error) => assert!(matches!(error.refusal(),Some(Refusal::NotNow(reason)) if reason.contains("continue the listing")), "{error:?}"),
                }
            }
            let payment =
                paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.0015"))
                    .with_options(
                        paykit_sdk::PayOptions::default()
                            .with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
                    );
            let paid = wallet.pay_without_request(payment).await.outcome.unwrap();
            assert!(
                matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
                "{paid:?}"
            );
            assert!(!wallet
                .app()
                .records_of("payer_activity")
                .await
                .unwrap()
                .is_empty());
            assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
            assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
        },
    ))
    .await;
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    shop.receive(None).await.unwrap();
    Box::pin(receives_resent_rejection(&mut wallet, &mine, &rejected)).await;
}

#[tokio::test]
async fn test_allowances_older_rejection_survives_renewal_without_closing_a_recreation() {
    // The approval app receives the rejection before it sees the later proposal.
    // A wider certificate waits for its keys. A completed listing still distinguishes the two lifetimes.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_app(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let me = wallet.app_id();
    let (proposal, id, rejection) = Box::pin(rejection_before_its_pending(
        &w,
        &mut wallet,
        &mut shop,
        AllowanceRole::Allower,
    ))
    .await;
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new()
            .allowance_execute()
            .request()
            .build()
            .unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let mine = Box::pin(records_proposal(&mut wallet, &proposal, &id)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    for _ in 0..2 {
        let summary = wallet.receive(None).await.unwrap();
        assert!(summary.scope_keys_awaited, "{summary:?}");
        assert_eq!(
            wallet
                .app()
                .protocol_scan("paykit", b"pr/")
                .await
                .unwrap()
                .len(),
            1
        );
        let record = wallet
            .app()
            .shared_record(&physical)
            .await
            .unwrap()
            .unwrap();
        let pending = paykit_lib::records::PendingAllowanceRecord::read(
            record.content.value().unwrap(),
            &wallet.identity(),
        )
        .unwrap();
        assert_eq!(pending.proposal().hash(), proposal.hash());
    }
    assert!(wallet.notices().await.unwrap().is_empty());
    store_bundle(&w, me, &wider);
    Box::pin(crate::common::finish_listing(&mut wallet)).await;
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        proposal.hash()
    );
    assert!(Box::pin(keeps(&wallet, b"pr/", &rejection)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_restore_own_rejection_waits_until_the_current_permanent_key_can_be_read() {
    use pubky_switch_client::app::Delivery;
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let shop_pending = pending_key(&shop, &w.alice_id(), &id);
    let a = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &id))
        .unwrap();
    let original = Box::pin(queued_rejection(&w, &mut wallet, &mine)).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    restored.receive(None).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(60_000);
        w.alice.switch().fail_next(Fault::Unreachable {
            path: format!("records/{}", pubky_switch_core::enc::b64(&a)),
            after: 0,
        });
        restored.app_mut().flush_message(&original).await.unwrap();
        assert!(matches!(
            restored.app().delivery(&original).await.unwrap(),
            Some(Delivery::Queued { .. })
        ));
        assert_eq!(
            Box::pin(taken(&mut shop, "paykit.allowance_rejection")).await,
            0
        );
        assert_eq!(
            shop.allowance(&theirs).await.unwrap().status,
            AllowanceStatus::Proposed
        );
    }
    w.clock.advance_ms(60_000);
    restored.app_mut().flush_message(&original).await.unwrap();
    let got = shop.receive(None).await.unwrap();
    assert!(got
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived { msg_id, .. } if msg_id == &original)));
    assert!(Box::pin(held_deleted(&shop, &shop_pending)).await);
    assert!(matches!(
        restored.app().delivery(&original).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    restored.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_rejection")).await,
        0
    );
}

/// Bob proposes an Allowance and alice's wallet accepts it. Her tool then writes the record
/// as a value no reader reads, before her phone has read anything: the phone holds the record
/// as that value. Returns the shop, the phone, the tool, the record keys on bob's side and on
/// alice's, the record's target and its value before.
async fn accepted_then_unreadable(
    w: &World,
) -> (
    Paykit,
    Paykit,
    Paykit,
    [u8; 32],
    [u8; 32],
    pubky_switch_client::app::RecordTarget,
    Vec<u8>,
) {
    let mut shop = Box::pin(shop(w)).await;
    let mut wallet = Box::pin(wallet(w)).await;
    let both = || AppScopes::new().pay().allowance().build().unwrap();
    let mut tool = Box::pin(alice_app(w, "tool", both())).await;
    let mut phone = Box::pin(alice_app(w, "phone", both())).await;
    let (theirs, mine) = Box::pin(proposed(w, &mut shop, &mut wallet)).await;
    Box::pin(accepted(w, &mut wallet, &mine, &mut shop, &theirs)).await;
    shop.receive(None).await.unwrap();
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let target = allowances_target(&w.bob_id(), &id);
    let held = tool.app_mut().read_record(&target).await.unwrap().unwrap();
    let good = held.content.value().unwrap().to_vec();
    let unreadable = serde_json::to_vec(&json!({ "v": 99 })).unwrap();
    Box::pin(crate::periods::writes_value(
        &mut tool,
        &target,
        &unreadable,
    ))
    .await;
    phone.receive(None).await.unwrap();
    assert_eq!(Box::pin(raw(&phone, &mine)).await, unreadable);
    (shop, phone, tool, theirs, mine, target, good)
}

#[tokio::test]
async fn test_allowances_unreadable_record_parks_the_original_proposal_on_first_read() {
    use pubky_switch_client::app::ParkReason;

    let w = World::new();
    let (shop, mut wallet, _, theirs, _, _, _) = Box::pin(accepted_then_unreadable(&w)).await;
    let id = shop.allowance(&theirs).await.unwrap().allowance_id;
    let accepted_at = w.clock.now_ms();
    let proposal = competing_proposal(&w, &shop, &wallet, &id);
    let report = wallet.receive(None).await.unwrap().report;
    let entries = wallet.app_mut().parked_entries().await.unwrap();
    let entry = entries.iter().find(|e| e.hash == proposal.hash()).unwrap();
    assert_eq!(entry.reason, ParkReason::ProtocolDeferred);
    assert!(report.cursor.seq >= entry.seq);
    let deferred = |stored_id| AppEvent::ProtocolDeferred {
        seq: entry.seq,
        stored_id,
        protocol: "paykit".into(),
        reason: "Allowance record unresolved",
    };
    assert!(report.events.contains(&deferred(None)), "{report:?}");
    let kept = wallet
        .app_mut()
        .parked_entry(entry.stored_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.accepted_at_ms, accepted_at);
    assert!(kept.sender_verified);
    assert_eq!(kept.sender, w.bob_id());
    assert_eq!(&kept.plaintext, proposal.plaintext());
    let retry = wallet.receive(None).await.unwrap().report;
    assert!(
        retry.events.contains(&deferred(Some(entry.stored_id))),
        "{retry:?}"
    );
}

#[tokio::test]
async fn test_allowances_unmatched_pending_proposal_keeps_the_original_replies() {
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (original, id) = unrecorded_proposal(&w, &wallet);
    let target = pending_target(&w.bob_id(), &id);
    let key = wallet.app().record_key(&target).unwrap();
    let permanent = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &id))
        .unwrap();

    let current = signed_pending_proposal(&wallet, w.bob_id(), &id, "allower");
    assert_ne!(original.hash(), current.hash());
    let value = PendingAllowanceRecord::new(&w.alice_id(), current)
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut wallet, &target, Some(&value))).await;
    Box::pin(read_listed_records(&mut wallet)).await;
    assert_eq!(Box::pin(raw(&wallet, &key)).await, value);
    let writes = w.record_writes(&w.alice, &key);
    for (kind, msg_id) in [
        ("paykit.allowance_acceptance", SENT_FIRST),
        ("paykit.allowance_rejection", SENT_SECOND),
    ] {
        answers_with_id(&w, &wallet, kind, &original, &id, msg_id);
        let read = wallet.receive(None).await.unwrap();
        assert!(read.applied.iter().any(|id| id == msg_id), "{read:?}");
        assert!(Box::pin(keeps(&wallet, b"pr/", msg_id)).await);
        assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    }
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    assert_eq!(Box::pin(raw(&wallet, &key)).await, value);
    assert!(wallet
        .app()
        .shared_record(&permanent)
        .await
        .unwrap()
        .is_none());
}

/// `signer`'s app `app` sends alice's wallet an end of the Allowance `mine` names, with
/// `refs`.
async fn end_with_refs(
    w: &World,
    signer: &pubky_switch_client::testing::TestSigner,
    app: &str,
    wallet: &Paykit,
    allowance_id: &paykit_lib::AllowanceId,
    refs: Vec<[u8; 32]>,
) -> String {
    w.clock.advance_ms(1);
    w.inject(
        signer,
        app,
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        refs,
        json!({ "allowance_id": allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    )
}

/// The `msg_id` of the end the wallet's record of the Allowance at `record` holds.
async fn recorded_end(w: &World, wallet: &Paykit, record: &[u8; 32]) -> Option<String> {
    let value = serde_json::to_vec(&Box::pin(value(wallet, record)).await).unwrap();
    AllowanceRecord::read(&value, &w.alice_id())
        .unwrap()
        .end
        .map(|end| end.message().msg_id.clone())
}

#[tokio::test]
async fn test_allowances_end_with_wrong_references_over_an_unreadable_record_keeps_a_later_one() {
    // While the wallet can't read the Allowance record, bob sends an end naming another
    // proposal, then his real end. Neither can be checked yet, so both are kept as stops, the
    // second not dropped for the first. Once the record reads, the end naming the record's
    // proposal and acceptance ends the Allowance and the other is dropped; a re-sent copy of
    // the real end changes nothing (Paykit spec 8.3, "Ends reach the executor").
    let w = World::new();
    let (mut shop, mut wallet, mut phone, theirs, mine, target, good) =
        Box::pin(accepted_then_unreadable(&w)).await;
    let id = shop.allowance(&theirs).await.unwrap().allowance_id;
    Box::pin(end_with_refs(
        &w,
        &w.bob,
        "shop",
        &wallet,
        &id,
        vec![[7; 32]],
    ))
    .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 1);
    w.clock.advance_ms(1);
    let ended = shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 2);
    Box::pin(crate::periods::writes_value(&mut phone, &target, &good)).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(
        Box::pin(recorded_end(&w, &wallet, &mine)).await,
        Some(ended.clone())
    );
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 0);
    // Bob's shop sends the real end again, asked for by its `msg_id`.
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.resend_request",
        Vec::new(),
        json!({ "msg_ids": [ended] }).as_object().cloned().unwrap(),
    );
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(
        Box::pin(recorded_end(&w, &wallet, &mine)).await,
        Some(ended)
    );
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 0);
}

#[tokio::test]
async fn test_allowances_third_identity_end_over_an_unreadable_record_keeps_no_stop_of_it() {
    // Carol, whom alice admits, sends an end under the Allowance ID of bob's Allowance with
    // made-up references while the wallet can't read the record. Its stop is under the record
    // key of an Allowance of carol's, which alice doesn't hold: it takes nothing of bob's
    // record, and bob's end, coming after it, ends the Allowance once the record reads.
    let w = World::new();
    let (mut shop, mut wallet, mut phone, theirs, mine, target, good) =
        Box::pin(accepted_then_unreadable(&w)).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut carol = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    carol.app_mut().invite(&w.alice_id()).await.unwrap();
    let id = shop.allowance(&theirs).await.unwrap().allowance_id;
    Box::pin(end_with_refs(
        &w,
        &w.carol,
        "shop",
        &wallet,
        &id,
        vec![[7; 32]],
    ))
    .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 0);
    assert_eq!(Box::pin(stops(&wallet)).await, 1);
    w.clock.advance_ms(1);
    let ended = shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 1);
    Box::pin(crate::periods::writes_value(&mut phone, &target, &good)).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(
        Box::pin(recorded_end(&w, &wallet, &mine)).await,
        Some(ended)
    );
}

#[tokio::test]
async fn test_allowances_second_end_over_an_unreadable_record_dropped_for_the_bound_asked_again() {
    // Over the record the wallet can't read, bob's end naming another proposal is kept as the
    // record's stop, outside the bounds; his real end after it counts. His 64 ends of
    // Allowances alice holds no record of then push the real end out: its pair is forgotten
    // and bob is asked at once. His re-sent copy is taken and kept again, and once the record
    // reads it ends the Allowance (Paykit spec 8.3, "Ends reach the executor"; Switch spec
    // 11).
    let w = World::new();
    let (mut shop, mut wallet, mut phone, theirs, mine, target, good) =
        Box::pin(accepted_then_unreadable(&w)).await;
    let id = shop.allowance(&theirs).await.unwrap().allowance_id;
    Box::pin(end_with_refs(
        &w,
        &w.bob,
        "shop",
        &wallet,
        &id,
        vec![[7; 32]],
    ))
    .await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(1);
    let ended = shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 2);
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 1);
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pd/")
            .await
            .unwrap()
            .len(),
        1
    );
    shop.receive(None).await.unwrap();
    let got = wallet.receive(None).await.unwrap();
    assert!(received(&got, "paykit.allowance_end"), "{got:?}");
    assert_eq!(Box::pin(ends_kept_at(&wallet, &mine)).await, 2);
    Box::pin(crate::periods::writes_value(&mut phone, &target, &good)).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(
        Box::pin(recorded_end(&w, &wallet, &mine)).await,
        Some(ended)
    );
}

#[tokio::test]
async fn test_allowances_owed_end_naming_another_end_is_corrupt() {
    // The owed mark of the keeper's crossing end, read back naming another `msg_id` than the
    // end its record holds, is state no writer leaves: the receive fails as a corrupt store,
    // and the mark is neither dropped nor taken as sent.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut wallet,
        SendsHeld::Blocked,
    ))
    .await;
    let (key, value) = wallet
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .remove(0);
    let mut owed: Value = serde_json::from_slice(&value).unwrap();
    owed["d"]["msg_id"] = json!("0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30");
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.clone(),
                value: serde_json::to_vec(&owed).unwrap(),
            }],
        )
        .await
        .unwrap();
    wallet.unblock(&w.bob_id()).await.unwrap();
    let error = wallet.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"eo/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(Box::pin(ends_naming_both(&wallet, &mine)).await);
}

/// As [`World::inject`], with `msg_id` as the message's ID.
#[allow(clippy::too_many_arguments)]
fn inject_with_id(
    w: &World,
    signer: &pubky_switch_client::testing::TestSigner,
    app: &str,
    targets: &[[u8; 16]],
    kind: &str,
    refs: Vec<[u8; 32]>,
    body: Value,
    msg_id: &str,
) {
    let keys = signer.identity().app_keys(app);
    let cert = signer.switch().app(&keys.app_id_bytes()).cert.unwrap();
    let mut message = pubky_switch_core::message::Message::new(
        kind,
        pubky_switch_core::message::Sender::of_app(signer.identity().public(), &keys),
        w.alice.identity().public(),
        w.clock.now_ms(),
        refs,
        body.as_object().cloned().unwrap(),
    )
    .unwrap();
    message.msg_id = msg_id.to_owned();
    let plaintext = message.sign(&keys).unwrap();
    w.alice
        .switch()
        .send_plaintext(signer.identity().public(), &cert, &plaintext, targets)
        .unwrap();
}

/// Carol's app `shop`, holding `paykit_request` and `paykit_allowance`, which alice's `wallet`
/// admits.
async fn carol_admitted(w: &World, wallet: &mut Paykit) -> Paykit {
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut carol = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    carol.app_mut().invite(&w.alice_id()).await.unwrap();
    carol
}

#[tokio::test]
async fn test_allowances_two_senders_ends_with_one_msg_id_kept_apart() {
    // Carol's end under the ID of bob's Allowance, with the `msg_id` bob's end will carry,
    // comes first. A message is its sender's and its `msg_id` (Switch spec 8.2): her stop is
    // under her own record key, and bob's end, with the same `msg_id`, ends his Allowance.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let _carol = Box::pin(carol_admitted(&w, &mut wallet)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    shop.receive(None).await.unwrap();
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&wallet, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    let id = "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30";
    let refs = vec![
        record.proposal.hash(),
        record.response.as_ref().unwrap().hash(),
    ];
    let body = json!({ "allowance_id": record.allowance_id.as_str() });
    w.clock.advance_ms(1);
    inject_with_id(
        &w,
        &w.carol,
        "shop",
        &[wallet.app_id()],
        "paykit.allowance_end",
        refs.clone(),
        body.clone(),
        id,
    );
    w.clock.advance_ms(1);
    inject_with_id(
        &w,
        &w.bob,
        "shop",
        &[wallet.app_id()],
        "paykit.allowance_end",
        refs,
        body,
        id,
    );
    let got = wallet.receive(None).await.unwrap();
    let ends = got
        .report
        .events
        .iter()
        .filter(|e| matches!(e, AppEvent::MessageReceived { kind, .. } if kind == "paykit.allowance_end"))
        .count();
    assert_eq!(ends, 2, "{got:?}");
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(
        Box::pin(recorded_end(&w, &wallet, &mine)).await,
        Some(id.to_owned())
    );
    let _ = theirs;
    assert_eq!(Box::pin(stops(&wallet)).await, 1);
    assert!(!Box::pin(keeps_end_at(&wallet, &mine)).await);
}

#[tokio::test]
async fn test_allowances_two_senders_answers_with_one_msg_id_kept_apart() {
    // Bob's and carol's acceptances of proposals the wallet holds no record of, under one
    // Allowance ID and one `msg_id`, are two messages: each is kept before its record, under
    // its sender (Switch spec 8.2; Paykit spec 8.1).
    let w = World::new();
    let shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let _carol = Box::pin(carol_admitted(&w, &mut wallet)).await;
    let id = "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30";
    let body = json!({ "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4" });
    for signer in [&w.bob, &w.carol] {
        w.clock.advance_ms(1);
        inject_with_id(
            &w,
            signer,
            "shop",
            &[wallet.app_id()],
            "paykit.allowance_acceptance",
            vec![[7; 32]],
            body.clone(),
            id,
        );
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    drop(shop);
}

/// Alice's wallet proposes an Allowance to bob as allower and bob's shop accepts it; her
/// `exec` app is its executor, consented by her `phone`. Bob is the proposal's recipient.
/// Returns the apps and the record keys on bob's side and on alice's.
async fn alice_proposed_executed(
    w: &World,
) -> (Paykit, Paykit, Paykit, Paykit, [u8; 32], [u8; 32]) {
    let mut shop = Box::pin(shop(w)).await;
    let mut wallet = Box::pin(paying_wallet(w)).await;
    let executor = AppScopes::new().allowance_execute().build().unwrap();
    let mut exec = Box::pin(alice_app(w, "exec", executor.clone())).await;
    let mut phone = Box::pin(alice_app(w, "phone", executor)).await;
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let mine = sent.record;
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    Box::pin(accepted(w, &mut shop, &theirs, &mut wallet, &mine)).await;
    wallet.receive(None).await.unwrap();
    exec.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    Box::pin(grant(w, &mut exec, &mine)).await;
    Box::pin(consent(&mut phone, &mine)).await;
    wallet.receive(None).await.unwrap();
    exec.receive(None).await.unwrap();
    assert!(exec.allowance(&mine).await.unwrap().consented);
    (shop, wallet, exec, phone, theirs, mine)
}

/// Bob's shop sends alice's `apps` a message of `kind` under the Allowance at `mine`, with
/// `refs` built from the record as `exec` holds it.
async fn bob_sends(
    w: &World,
    exec: &Paykit,
    apps: &[[u8; 16]],
    mine: &[u8; 32],
    kind: &str,
    refs: impl FnOnce(&AllowanceRecord) -> Vec<[u8; 32]>,
) {
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(exec, mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    w.clock.advance_ms(1);
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        apps,
        kind,
        refs(&record),
        json!({ "allowance_id": record.allowance_id.as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
}

#[tokio::test]
async fn test_allowances_recipient_end_naming_the_proposal_alone_leaves_consent() {
    // An end naming the proposal alone is the proposal's withdrawal by its sender (Paykit
    // wire 4.16). Bob received alice's proposal and accepted it: his end naming the proposal
    // alone is no end of this Allowance. The executor's consent stays, nothing is recorded,
    // and no stop is left.
    let w = World::new();
    let (_shop, wallet, mut exec, _phone, _, mine) = Box::pin(alice_proposed_executed(&w)).await;
    Box::pin(bob_sends(
        &w,
        &exec,
        &[exec.app_id(), wallet.app_id()],
        &mine,
        "paykit.allowance_end",
        |record| vec![record.proposal.hash()],
    ))
    .await;
    let got = exec.receive(None).await.unwrap();
    assert!(received(&got, "paykit.allowance_end"), "{got:?}");
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "held": { "confirm_again": false } }))
    );
    assert!(exec.allowance(&mine).await.unwrap().consented);
    assert!(Box::pin(value(&exec, &mine)).await["end"].is_null());
    assert!(exec
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_allowances_second_response_naming_another_proposal_keeps_no_conflict() {
    // Bob accepted alice's proposal; a rejection of his naming another proposal answers no
    // proposal of this Allowance (Paykit wire 4.16): it is dropped, with no conflict kept,
    // nothing written, and the executor's consent unchanged.
    let w = World::new();
    let (_shop, mut wallet, mut exec, _phone, _, mine) =
        Box::pin(alice_proposed_executed(&w)).await;
    Box::pin(bob_sends(
        &w,
        &exec,
        &[exec.app_id(), wallet.app_id()],
        &mine,
        "paykit.allowance_rejection",
        |_| vec![[7; 32]],
    ))
    .await;
    exec.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    for app in [&exec, &wallet] {
        assert!(app
            .app()
            .protocol_scan("paykit", b"cf/")
            .await
            .unwrap()
            .is_empty());
        assert!(Box::pin(value(app, &mine)).await["conflicted"].is_null());
        assert_eq!(
            app.allowance(&mine).await.unwrap().status,
            AllowanceStatus::Accepted
        );
    }
    assert_eq!(
        Box::pin(consent_entry(&exec, &mine)).await,
        Some(json!({ "held": { "confirm_again": false } }))
    );
    assert!(exec.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_restored_end_acceptance_time_ahead_is_clamped() {
    // A kept end dated a day ahead of anything the backup verified is restored with its
    // acceptance time brought down to that time plus the clock tolerance, as a kept
    // cancellation is: dated there, it would never go (Paykit spec 7.2, 8.3).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    let backup: Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    drop(wallet);
    let mut state = crate::restore::layer_of(&backup);
    let end = state
        .iter_mut()
        .find(|(k, _)| k.starts_with(b"ae/"))
        .expect("the backup keeps the end");
    end.1["d"]["accepted_at"] = json!(w.clock.now_ms() + 24 * HOUR_MS);
    let backup = paykit_sdk::PaykitBackup::from_bytes(
        &serde_json::to_vec(&crate::restore::with_layer(&backup, state)).unwrap(),
    )
    .unwrap();
    let restored = Box::pin(restored_unreceived(&w, &backup)).await;
    let bound = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    let (_, value) = restored
        .app()
        .protocol_scan("paykit", b"ae/")
        .await
        .unwrap()
        .remove(0);
    let value: Value = serde_json::from_slice(&value).unwrap();
    assert!(
        value["d"]["accepted_at"].as_u64().unwrap() <= bound,
        "{value}"
    );
}

/// Two message IDs whose lexical order is the reverse of the order they are sent in.
const SENT_FIRST: &str = "f0e1d2c3-b4a5-4697-8879-6a5b4c3d2e1f";
const SENT_SECOND: &str = "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30";

#[tokio::test]
async fn test_allowances_ends_of_one_millisecond_recorded_in_acceptance_order() {
    // Over the record the phone can't read, bob's two ends naming the proposal and the
    // acceptance come in one millisecond, the one whose `msg_id` sorts last first. Once the
    // record reads, the one the inbox accepted first ends the Allowance, not the one whose
    // `msg_id` sorts first (Paykit spec 8.1, 8.3).
    let w = World::new();
    let (_shop, mut phone, mut tool, _, mine, target, good) =
        Box::pin(accepted_then_unreadable(&w)).await;
    let record = AllowanceRecord::read(&good, &w.alice_id()).unwrap();
    let refs = vec![
        record.proposal.hash(),
        record.response.as_ref().unwrap().hash(),
    ];
    let body = json!({ "allowance_id": record.allowance_id.as_str() });
    w.clock.advance_ms(1);
    for id in [SENT_FIRST, SENT_SECOND] {
        inject_with_id(
            &w,
            &w.bob,
            "shop",
            &[phone.app_id()],
            "paykit.allowance_end",
            refs.clone(),
            body.clone(),
            id,
        );
    }
    phone.receive(None).await.unwrap();
    assert_eq!(Box::pin(ends_kept_at(&phone, &mine)).await, 2);
    Box::pin(crate::periods::writes_value(&mut tool, &target, &good)).await;
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(
        Box::pin(recorded_end(&w, &phone, &mine)).await,
        Some(SENT_FIRST.to_owned())
    );
}

#[tokio::test]
async fn test_allowances_answers_of_one_millisecond_placed_in_acceptance_order() {
    // The inbox received the rejection before the acceptance in the same millisecond.
    // A pending proposal first recorded after both replies cannot be closed by that rejection.
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (proposal, id) = unrecorded_proposal(&w, &wallet);
    let answer = json!({ "allowance_id": id.as_str() });
    w.clock.advance_ms(1);
    for (kind, msg_id) in [
        ("paykit.allowance_rejection", SENT_FIRST),
        ("paykit.allowance_acceptance", SENT_SECOND),
    ] {
        inject_with_id(
            &w,
            &w.bob,
            "shop",
            &[wallet.app_id()],
            kind,
            vec![proposal.hash()],
            answer.clone(),
            msg_id,
        );
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    let mine = Box::pin(records_proposal(&mut wallet, &proposal, &id)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        proposal.hash()
    );
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_SECOND)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
}

/// A fourth identity, dave, with its own switch, and an app of his, `name`, with `scopes`.
pub(super) async fn dave_app(
    w: &World,
    name: &str,
    scopes: RequestedScopes,
) -> (pubky_switch_client::testing::TestSigner, Paykit) {
    let dave = pubky_switch_client::testing::TestSigner::at(
        pubky_switch_client::testing::TestIdentity::new(
            &pubky_switch_client::testing::Seeds::default(),
            "dave",
        ),
        "https://dave-switch.example",
        std::sync::Arc::clone(&w.clock),
        std::sync::Arc::clone(&w.homeserver),
        crate::common::spec(),
    );
    w.router.add_switch(dave.origin(), dave.switch());
    let app = w.join(&dave, name, scopes).await;
    (dave, app)
}

/// `signer`'s app `app` sends `to`'s app `target` a message of `kind`, with `refs` and `body`,
/// in the current millisecond. Returns its `msg_id`.
#[allow(clippy::too_many_arguments)]
fn send(
    w: &World,
    signer: &pubky_switch_client::testing::TestSigner,
    app: &str,
    to: &pubky_switch_client::testing::TestSigner,
    target: [u8; 16],
    kind: &str,
    refs: Vec<[u8; 32]>,
    body: Value,
) -> String {
    w.inject(
        signer,
        app,
        to,
        &[target],
        kind,
        refs,
        body.as_object().cloned().unwrap(),
    )
}

/// Whether `app` keeps a message `msg_id` under `prefix`.
async fn keeps(app: &Paykit, prefix: &[u8], msg_id: &str) -> bool {
    app.app()
        .protocol_scan("paykit", prefix)
        .await
        .unwrap()
        .iter()
        .any(|(key, _)| String::from_utf8_lossy(key).ends_with(msg_id))
}

/// Alice's wallet, in one millisecond, sends bob's shop an acceptance of a proposal the shop
/// holds no record of, then an acceptance of a request it holds none of: both are kept
/// before their records, in one pool (`pr/`). Then `more` acceptances of other requests,
/// each a millisecond later. Returns the two first `msg_id`s.
fn replies_tied_then(w: &World, shop: &Paykit, more: usize) -> (String, String) {
    let allowance = send(
        w,
        &w.alice,
        "wallet",
        &w.bob,
        shop.app_id(),
        "paykit.allowance_acceptance",
        vec![[0xaa; 32]],
        json!({ "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4" }),
    );
    let request = send(
        w,
        &w.alice,
        "wallet",
        &w.bob,
        shop.app_id(),
        "paykit.payment_request_acceptance",
        vec![[0; 32]],
        json!({}),
    );
    for i in 0..more {
        w.clock.advance_ms(1);
        send(
            w,
            &w.alice,
            "wallet",
            &w.bob,
            shop.app_id(),
            "paykit.payment_request_acceptance",
            vec![[u8::try_from(i + 1).unwrap(); 32]],
            json!({}),
        );
    }
    (allowance, request)
}

#[tokio::test]
async fn test_allowances_reply_pool_drops_the_first_accepted_of_one_millisecond_per_sender() {
    // An Allowance acceptance and a request's acceptance from alice share a millisecond, the
    // Allowance's accepted first, and 62 more of hers come later: 64 kept replies of one
    // sender. One more drops the one her inbox accepted first, the Allowance's, whatever the
    // family (Paykit spec 8.1, "Replies before their record").
    let w = World::new();
    let shop = Box::pin(shop(&w)).await;
    let mut shop = shop;
    let _wallet = Box::pin(wallet(&w)).await;
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (allowance, request) = replies_tied_then(&w, &shop, bound - 2);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        bound
    );
    w.clock.advance_ms(1);
    send(
        &w,
        &w.alice,
        "wallet",
        &w.bob,
        shop.app_id(),
        "paykit.payment_request_acceptance",
        vec![[0xbb; 32]],
        json!({}),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        bound
    );
    assert!(!Box::pin(keeps(&shop, b"pr/", &allowance)).await);
    assert!(Box::pin(keeps(&shop, b"pr/", &request)).await);
}

#[tokio::test]
async fn test_allowances_reply_pool_drops_the_first_accepted_of_one_millisecond_in_all() {
    // The same tie in alice's 64 kept replies, with carol's 63 and one of dave's: 128 in
    // all. Dave's second drops the earliest of the sender holding the most, alice: the one
    // her inbox accepted first, the Allowance's (Paykit spec 8.1).
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let _wallet = Box::pin(wallet(&w)).await;
    let payer = || AppScopes::new().pay().build().unwrap();
    let mut carol = w.join(&w.carol, "wallet", payer()).await;
    carol.app_mut().invite(&w.bob_id()).await.unwrap();
    let (dave, mut dave_wallet) = Box::pin(dave_app(&w, "wallet", payer())).await;
    dave_wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut().invite(&w.carol_id()).await.unwrap();
    shop.app_mut()
        .invite(&dave.identity().public())
        .await
        .unwrap();
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (allowance, request) = replies_tied_then(&w, &shop, bound - 2);
    for (from, count, base) in [(&w.carol, bound - 1, 0x40u8), (&dave, 1, 0x90)] {
        for i in 0..count {
            w.clock.advance_ms(1);
            send(
                &w,
                from,
                "wallet",
                &w.bob,
                shop.app_id(),
                "paykit.payment_request_acceptance",
                vec![[base.wrapping_add(u8::try_from(i).unwrap()); 32]],
                json!({}),
            );
        }
    }
    shop.receive(None).await.unwrap();
    let all = paykit_sdk::MAX_KEPT_REPLIES;
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        all
    );
    w.clock.advance_ms(1);
    send(
        &w,
        &dave,
        "wallet",
        &w.bob,
        shop.app_id(),
        "paykit.payment_request_acceptance",
        vec![[0xfe; 32]],
        json!({}),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        all
    );
    assert!(!Box::pin(keeps(&shop, b"pr/", &allowance)).await);
    assert!(Box::pin(keeps(&shop, b"pr/", &request)).await);
}

/// `from`'s app `app` ends alice's wallet's Allowance number `i`, which she holds no record
/// of, in the current millisecond. Returns its `msg_id`.
fn end_from(
    w: &World,
    from: &pubky_switch_client::testing::TestSigner,
    app: &str,
    wallet: &Paykit,
    i: usize,
) -> String {
    send(
        w,
        from,
        app,
        &w.alice,
        wallet.app_id(),
        "paykit.allowance_end",
        vec![[0xee; 32]],
        json!({ "allowance_id": format!("d0efffb7-191b-4709-b6c7-e343af55{i:04x}") }),
    )
}

/// Bob, in one millisecond, ends an Allowance the wallet holds no record of, then cancels a
/// request it holds none of: both are kept as stops, in one pool. Then `more` ends, each a
/// millisecond later. Returns the two first `msg_id`s.
fn stops_tied_then(w: &World, wallet: &Paykit, more: usize) -> (String, String) {
    let end = end_from(w, &w.bob, "shop", wallet, 0);
    let cancellation = crate::one_time::cancel_to(w, &[wallet.app_id()], [0xcc; 32]);
    for i in 0..more {
        w.clock.advance_ms(1);
        end_from(w, &w.bob, "shop", wallet, i + 1);
    }
    (end, cancellation)
}

#[tokio::test]
async fn test_allowances_stop_pool_drops_the_first_accepted_of_one_millisecond_per_sender() {
    // Bob's end and his cancellation share a millisecond, the end accepted first, and 62
    // more of his ends come later: 64 stops of one sender. One more drops the one his inbox
    // accepted first, the end, whatever the family (Paykit spec 8.1, 8.3).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (end, cancellation) = stops_tied_then(&w, &wallet, bound - 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound);
    w.clock.advance_ms(1);
    end_from(&w, &w.bob, "shop", &wallet, bound);
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound);
    assert!(!Box::pin(keeps(&wallet, b"ae/", &end)).await);
    assert!(!wallet
        .app()
        .protocol_scan("paykit", b"kc/")
        .await
        .unwrap()
        .is_empty());
    let _ = cancellation;
}

#[tokio::test]
async fn test_allowances_stop_pool_drops_the_first_accepted_of_one_millisecond_in_all() {
    // The same tie in bob's 64 stops, with carol's 63 ends and one of dave's: 128 in all.
    // Dave's second drops the earliest of the sender holding the most, bob: the one his
    // inbox accepted first, the end (Paykit spec 8.1, 8.3).
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let allowance = || AppScopes::new().allowance().build().unwrap();
    let mut carol = w.join(&w.carol, "shop", allowance()).await;
    carol.app_mut().invite(&w.alice_id()).await.unwrap();
    let (dave, mut dave_shop) = Box::pin(dave_app(&w, "shop", allowance())).await;
    dave_shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet
        .app_mut()
        .invite(&dave.identity().public())
        .await
        .unwrap();
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (end, _) = stops_tied_then(&w, &wallet, bound - 2);
    for i in 0..bound - 1 {
        w.clock.advance_ms(1);
        end_from(&w, &w.carol, "shop", &wallet, 0x100 + i);
    }
    w.clock.advance_ms(1);
    end_from(&w, &dave, "shop", &wallet, 0x200);
    wallet.receive(None).await.unwrap();
    let all = paykit_sdk::MAX_KEPT_REPLIES;
    assert_eq!(Box::pin(stops(&wallet)).await, all);
    w.clock.advance_ms(1);
    end_from(&w, &dave, "shop", &wallet, 0x201);
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, all);
    assert!(!Box::pin(keeps(&wallet, b"ae/", &end)).await);
    assert!(!wallet
        .app()
        .protocol_scan("paykit", b"kc/")
        .await
        .unwrap()
        .is_empty());
}

/// An app of alice's, `name`, with `scopes`, on `store`; she admits bob.
async fn alice_on(
    w: &World,
    name: &str,
    scopes: RequestedScopes,
    store: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) -> Paykit {
    let mut app = w
        .join_with(
            &w.alice,
            name,
            scopes,
            paykit_sdk::PaykitConfig::default(),
            store,
        )
        .await;
    app.app_mut().invite(&w.bob_id()).await.unwrap();
    app
}

#[tokio::test]
async fn test_allowances_proposal_received_over_a_divergent_record_kept_as_the_conflict_at_once() {
    // A proposal under the ID of an Allowance whose record the wallet holds writes over it
    // only as a conflict, a stop: over a divergent record it is kept as the stop at once and
    // written into `conflicted`, never waiting for the confirmation (Paykit spec 6, 8.3).
    let w = World::new();
    let base = crate::common::store();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    crate::divergent::set_divergent(&base, &wallet, &mine, true);
    w.clock.advance_ms(1);
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
}

/// A current covering service read clears a previous divergent view of this pending record.
async fn read_current_proposal(wallet: &mut Paykit, peer: &[u8; 32], id: &paykit_lib::AllowanceId) {
    let target = pending_target(peer, id);
    let observed = wallet.app_mut().read_record_fresh(&target).await.unwrap();
    assert!(
        matches!(&observed, pubky_switch_client::app::RecordRead::Fresh(record)
        if record.authenticated && !record.doubtful && !record.divergent && !record.flagged && !record.not_taken
        && record.content.value().is_some()),
        "{observed:?}"
    );
}

/// Alice's wallet, on `base`, holding bob's proposal, the record's view set divergent.
async fn proposed_over_divergent(
    w: &World,
) -> (
    Paykit,
    Paykit,
    [u8; 32],
    [u8; 32],
    std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) {
    let base = crate::common::store();
    let mut shop = Box::pin(shop(w)).await;
    let mut wallet = Box::pin(alice_on(
        w,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let (theirs, mine) = Box::pin(proposed(w, &mut shop, &mut wallet)).await;
    (shop, wallet, theirs, mine, base)
}

#[tokio::test]
async fn test_allowances_user_rejection_waits_for_a_current_pending_record() {
    // The user's rejection closes the selected pending proposal only after a covering read.
    let w = World::new();
    let (mut shop, mut wallet, _, mine, base) = Box::pin(proposed_over_divergent(&w)).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    crate::divergent::set_divergent(&base, &wallet, &physical, true);
    wallet.receive(None).await.unwrap();
    let before = w.record_writes(&w.alice, &physical);
    let held = wallet.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(held.refusal(), Some(Refusal::NotNow(_))),
        "{held:?}"
    );
    assert_eq!(w.record_writes(&w.alice, &physical), before);
    Box::pin(read_current_proposal(&mut wallet, &w.bob_id(), &id)).await;
    wallet.reject_allowance(&mine).await.unwrap();
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert!(wallet
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_rejection")).await,
        1
    );
}

#[tokio::test]
async fn test_allowances_end_goes_over_a_divergent_record() {
    // An end is a stop: over a record seen divergent and unconfirmed, bob's end of the
    // Allowance the wallet accepted is recorded at once, and so is the wallet's own end of
    // it, sent (Paykit spec 6).
    let w = World::new();
    let (mut shop, mut wallet, theirs, mine, base) = Box::pin(proposed_over_divergent(&w)).await;
    Box::pin(accept_elsewhere(&w, &mut wallet, &mine)).await;
    shop.receive(None).await.unwrap();
    crate::divergent::set_divergent(&base, &wallet, &mine, true);
    wallet.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(!Box::pin(value(&wallet, &mine)).await["end"].is_null());
}

#[tokio::test]
async fn test_allowances_received_rejection_is_kept_until_its_pending_record_is_current() {
    // The wallet keeps Bob's original rejection while its proposal view is divergent.
    // It applies the conditional deletion once the service confirms the current record.
    let w = World::new();
    let base = crate::common::store();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let physical = pending_key(&wallet, &w.bob_id(), &sent.allowance_id);
    crate::divergent::set_divergent(&base, &wallet, &physical, true);
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    let r = shop.reject_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &r)).await);
    Box::pin(read_current_proposal(
        &mut wallet,
        &w.bob_id(),
        &sent.allowance_id,
    ))
    .await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert!(wallet
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
}

/// Alice's `wallet` signs a proposal to bob and writes no record of it, as another own app
/// of hers would send it. Returns the proposal as its record stores it, and its Allowance.
fn unrecorded_proposal(
    w: &World,
    wallet: &Paykit,
) -> (paykit_lib::records::StoredMessage, paykit_lib::AllowanceId) {
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let body = json!({
        "allowance_id": id.as_str(),
        "proposer_role": "allower", "allower_generation": 1,
        "terms": terms().to_json().unwrap(),
    });
    let signed = wallet
        .app()
        .sign_message(pubky_switch_client::app::OutgoingMessage::new(
            w.bob_id(),
            "paykit.allowance_proposal",
            body.as_object().cloned().unwrap(),
        ))
        .unwrap();
    let proposal = paykit_lib::records::StoredMessage::new(
        w.alice_id(),
        wallet.app().cert().signed(),
        signed.plaintext().clone(),
        None,
        &w.bob_id(),
    )
    .unwrap();
    (proposal, id)
}

/// The record of `proposal` alone is written by `wallet`, as the app that sent it would
/// write it. Returns its record key.
async fn records_proposal(
    wallet: &mut Paykit,
    proposal: &paykit_lib::records::StoredMessage,
    id: &paykit_lib::AllowanceId,
) -> [u8; 32] {
    use pubky_switch_core::ops::{Condition, RecordVersion};
    let own = wallet.identity();
    let record = PendingAllowanceRecord::new(&own, proposal.clone()).unwrap();
    let peer = *record.counterparty();
    let pending = pending_target(&peer, id);
    let canonical = wallet
        .app()
        .record_key(&allowances_target(&peer, id))
        .unwrap();
    assert_eq!(record.allowance_id(), id);
    let op = wallet
        .app_mut()
        .record_op(
            &pending,
            Some(&record.to_json().unwrap()),
            vec![Condition::RecordVersion {
                key: canonical,
                version: RecordVersion::NONE,
            }],
        )
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    canonical
}

/// Bob's answer of `kind` to `proposal`, under `msg_id`, sent to alice's app `to`.
fn answers_with_id(
    w: &World,
    to: &Paykit,
    kind: &str,
    proposal: &paykit_lib::records::StoredMessage,
    id: &paykit_lib::AllowanceId,
    msg_id: &str,
) {
    inject_with_id(
        w,
        &w.bob,
        "shop",
        &[to.app_id()],
        kind,
        vec![proposal.hash()],
        json!({ "allowance_id": id.as_str() }),
        msg_id,
    );
}

/// The `msg_id`s of the response and of the `conflicted` message the record at `record`
/// holds, as `app` reads it.
async fn response_and_conflicted(
    w: &World,
    app: &Paykit,
    record: &[u8; 32],
) -> (Option<String>, Option<String>) {
    let held = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(app, record)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    let id_of =
        |m: Option<&paykit_lib::records::StoredMessage>| m.map(|m| m.message().msg_id.clone());
    (
        id_of(held.response.as_ref()),
        id_of(held.conflicted.as_ref()),
    )
}

/// How many messages `app` keeps under `prefix`.
async fn kept_under(app: &Paykit, prefix: &[u8]) -> usize {
    app.app()
        .protocol_scan("paykit", prefix)
        .await
        .unwrap()
        .len()
}

/// The acceptance arrives before the proposal. The later rejection arrives either before
/// or after the wallet records P. Both retain their order through a divergent view and reload.
async fn rejection_behind_a_waiting_acceptance(rejection_after_record: bool) {
    let w = World::new();
    let base = crate::common::store();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let (proposal, id) = unrecorded_proposal(&w, &wallet);
    w.clock.advance_ms(1);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_acceptance",
        &proposal,
        &id,
        SENT_FIRST,
    );
    let rejects = |wallet: &Paykit| {
        answers_with_id(
            &w,
            wallet,
            "paykit.allowance_rejection",
            &proposal,
            &id,
            SENT_SECOND,
        );
    };
    if !rejection_after_record {
        rejects(&wallet);
    }
    wallet.receive(None).await.unwrap();
    let kept = if rejection_after_record { 1 } else { 2 };
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, kept);
    let mine = Box::pin(records_proposal(&mut wallet, &proposal, &id)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    crate::divergent::set_divergent(&base, &wallet, &physical, true);
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = Box::pin(w.load_on_clock(
        &w.alice,
        std::sync::Arc::clone(&base),
        app_id,
        std::sync::Arc::clone(&w.clock),
    ))
    .await;
    assert!(wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .any(|v| v.record == physical && v.paused));
    if rejection_after_record {
        wallet.receive(None).await.unwrap();
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        rejects(&wallet);
    }
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        assert!(wallet
            .app()
            .shared_record(&physical)
            .await
            .unwrap()
            .unwrap()
            .content
            .value()
            .is_some());

        assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    }
    crate::divergent::confirm(&mut wallet, &physical)
        .await
        .unwrap();
    assert!(wallet.allowance(&mine).await.is_err());
    Box::pin(read_current_proposal(&mut wallet, &w.bob_id(), &id)).await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.allowance(&mine).await.unwrap().status,
            AllowanceStatus::Conflicted
        );
    }
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert_eq!(Box::pin(kept_under(&wallet, b"pc/")).await, 1);
}

/// A record key as the keys of the app's own state spell it.
fn b64_of(record: &[u8; 32]) -> String {
    pubky_switch_core::enc::b64(record)
}

#[tokio::test]
async fn test_allowances_kept_rejection_never_takes_the_response_of_an_earlier_kept_acceptance() {
    Box::pin(rejection_behind_a_waiting_acceptance(false)).await;
}

#[tokio::test]
async fn test_allowances_received_rejection_never_takes_the_response_of_a_kept_acceptance() {
    Box::pin(rejection_behind_a_waiting_acceptance(true)).await;
}

#[tokio::test]
async fn test_allowances_received_rejection_never_takes_the_response_of_a_waiting_acceptance() {
    // Alice receives Bob's acceptance and then his rejection while the proposal is divergent.
    // After a covering read she retains the conflict and leaves the permanent record unopened.
    let w = World::new();
    let base = crate::common::store();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let physical = pending_key(&wallet, &w.bob_id(), &sent.allowance_id);
    let proposal = Box::pin(pending_at(&wallet, &sent.record))
        .await
        .proposal()
        .clone();
    crate::divergent::set_divergent(&base, &wallet, &physical, true);
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = Box::pin(w.load_on_clock(
        &w.alice,
        std::sync::Arc::clone(&base),
        app_id,
        std::sync::Arc::clone(&w.clock),
    ))
    .await;
    assert!(wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .any(|v| v.record == physical && v.paused));
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    Box::pin(accept_elsewhere(&w, &mut shop, &theirs)).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .shared_record(&sent.record)
        .await
        .unwrap()
        .is_none());
    w.clock.advance_ms(1);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &sent.allowance_id,
        SENT_SECOND,
    );
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        assert!(wallet
            .app()
            .shared_record(&physical)
            .await
            .unwrap()
            .unwrap()
            .content
            .value()
            .is_some());

        assert!(wallet
            .app()
            .shared_record(&sent.record)
            .await
            .unwrap()
            .is_none());
    }
    crate::divergent::confirm(&mut wallet, &physical)
        .await
        .unwrap();
    assert!(wallet.allowance(&sent.record).await.is_err());
    Box::pin(read_current_proposal(
        &mut wallet,
        &w.bob_id(),
        &sent.allowance_id,
    ))
    .await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.allowance(&sent.record).await.unwrap().status,
            AllowanceStatus::Conflicted
        );
    }
    assert!(wallet
        .app()
        .shared_record(&sent.record)
        .await
        .unwrap()
        .is_none());
}

/// A `msg_id` that sorts before any generated one.
const SORTS_FIRST: &str = "00000000-0000-4000-8000-000000000001";

#[tokio::test]
async fn test_allowances_refused_answers_of_one_millisecond_placed_in_acceptance_order() {
    // Two rejections share a millisecond. The earlier one retains its place through
    // the refused conditional delete. The wallet closes the original proposal once.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let theirs = shop.allowances().await.unwrap()[0].record;
    let first = shop.reject_allowance(&theirs).await.unwrap();
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SORTS_FIRST,
    );
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    refuse_writes(&w, 1);
    let received = Box::pin(received_originals(&mut wallet, &[&first, SORTS_FIRST])).await;
    assert_eq!(received, [first.as_str(), SORTS_FIRST]);
    assert!(Box::pin(keeps(&wallet, b"pr/", &first)).await);
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        proposal.hash()
    );
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(!Box::pin(keeps(&wallet, b"pr/", &first)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
}

/// Fail the next `n` batches the wallet sends with `code`.
fn fail_writes_with(w: &World, n: usize, code: pubky_switch_core::http::ErrorCode) {
    for _ in 0..n {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code,
                current: Value::Null,
            });
    }
}

/// Alice's proposal, recorded, and the record as her wallet holds it.
async fn recorded_proposal_of(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
) -> (
    [u8; 32],
    paykit_lib::records::StoredMessage,
    paykit_lib::AllowanceId,
) {
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let held = Box::pin(pending_at(wallet, &sent.record)).await;
    (sent.record, held.proposal().clone(), sent.allowance_id)
}

#[tokio::test]
async fn test_allowances_refused_answer_ranks_before_a_later_kept_answer_of_one_millisecond() {
    // The first rejection was refused, then the sibling's delete retired it. A second
    // rejection received in that same millisecond after the delete cannot close a later create.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_FIRST,
    );
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    Box::pin(delete_pending_record(&w, &id)).await;
    wallet.receive(None).await.unwrap();
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SORTS_FIRST,
    );
    wallet.receive(None).await.unwrap();
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert!(Box::pin(keeps(&wallet, b"pr/", SORTS_FIRST)).await);
    assert_eq!(
        Box::pin(records_proposal(&mut wallet, &proposal, &id)).await,
        mine
    );
    let writes = w.record_writes(&w.alice, &physical);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        proposal.hash()
    );
    assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
}

#[tokio::test]
async fn test_allowances_refused_answer_keeps_a_later_answer_of_the_same_read_out_of_the_response()
{
    // Two rejections share a millisecond. The earlier one retains its place through
    // the refused conditional delete. The wallet closes the original proposal once.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let first = SENT_FIRST.to_owned();
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        &first,
    );
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SORTS_FIRST,
    );
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    fail_writes_with(&w, 1, pubky_switch_core::http::ErrorCode::TooLarge);
    let received = Box::pin(received_originals(&mut wallet, &[&first, SORTS_FIRST])).await;
    assert_eq!(received, [first.as_str(), SORTS_FIRST]);
    assert!(Box::pin(keeps(&wallet, b"pr/", &first)).await);
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        proposal.hash()
    );
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(!Box::pin(keeps(&wallet, b"pr/", &first)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
}

#[tokio::test]
async fn test_allowances_first_rejection_deletes_the_proposal_before_receive_returns() {
    // The first rejection closes the exact pending lifetime during this read.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_FIRST,
    );
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    let received = wallet.receive(None).await.unwrap();
    assert!(received.report.events.iter().any(
        |event| matches!(event, AppEvent::MessageReceived { msg_id, .. } if msg_id == SENT_FIRST)
    ));
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, 0);
}

#[tokio::test]
async fn test_allowances_two_rejections_of_one_read_close_the_original_once() {
    // Two rejections have the same acceptance time and opposite message-ID order.
    // Their original positions order them. Only one delete may close their pending record.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    for (kind, msg_id) in [
        ("paykit.allowance_rejection", SENT_FIRST),
        ("paykit.allowance_rejection", SORTS_FIRST),
    ] {
        answers_with_id(&w, &wallet, kind, &proposal, &id, msg_id);
    }
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    let accepted = Box::pin(received_originals(&mut wallet, &[SENT_FIRST, SORTS_FIRST])).await;
    assert_eq!(accepted, [SENT_FIRST, SORTS_FIRST]);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert!(matches!(
        wallet.allowance(&mine).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
}

/// Bob's first rejection, received by alice's wallet with its store stopping at `fault`, then
/// read again by the wallet reloaded from what was committed. Returns whether the fault was
/// hit, and whether the reloaded wallet found the first rejection kept to write and not yet in the
/// record: the receive step's commit kept it, and the placement after the read hadn't run.
async fn rejection_kept_through_a_stop(fault: pubky_switch_client::store::Fault) -> (bool, bool) {
    let w = World::new();
    let base = crate::common::store();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_FIRST,
    );
    let app_id = wallet.app_id();
    drop(wallet);
    let faulty = std::sync::Arc::new(pubky_switch_client::store::FaultStore::new(
        std::sync::Arc::clone(&base),
        fault,
    ));
    let mut crashing = Box::pin(w.load_on_clock(
        &w.alice,
        std::sync::Arc::clone(&faulty)
            as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        app_id,
        std::sync::Arc::clone(&w.clock),
    ))
    .await;
    let _ = crashing.receive(None).await;
    let hit = faulty.stopped();
    drop(crashing);
    let mut wallet = Box::pin(w.load_on_clock(
        &w.alice,
        std::sync::Arc::clone(&base),
        app_id,
        std::sync::Arc::clone(&w.clock),
    ))
    .await;
    let between = Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await
        && wallet
            .app()
            .shared_record(&physical)
            .await
            .unwrap()
            .is_some_and(|held| held.authenticated && held.content.value().is_some());
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(
        Box::pin(held_deleted(&wallet, &physical)).await,
        "{fault:?}"
    );
    assert_eq!(
        w.record_writes(&w.alice, &physical),
        writes + 1,
        "{fault:?}"
    );
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, 0, "{fault:?}");
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert!(matches!(
        wallet.allowance(&mine).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    (hit, between)
}

#[tokio::test]
async fn test_allowances_answer_kept_through_a_stop_after_every_commit_placed_once() {
    // A stop after each commit of the receive, the receive step's keep included: the next
    // receive places the first rejection kept, once, and nothing is lost.
    let mut n = 1;
    let mut seen_between = false;
    loop {
        let (hit, between) = Box::pin(rejection_kept_through_a_stop(
            pubky_switch_client::store::Fault::StopAfter(n),
        ))
        .await;
        seen_between |= between;
        if !hit {
            break;
        }
        n += 1;
        assert!(n < 64, "the receive never finished");
    }
    assert!(
        seen_between,
        "a stop between the keep and the placement leaves the acceptance kept"
    );
}

#[tokio::test]
async fn test_allowances_answer_over_a_held_record_never_dropped_for_the_bounds() {
    // The first rejection of a held proposal survives a full group of unrelated replies.
    // A second rejection in the same feed still results in one conditional deletion.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_FIRST,
    );
    for i in 0..paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        send(
            &w,
            &w.bob,
            "shop",
            &w.alice,
            wallet.app_id(),
            "paykit.allowance_rejection",
            vec![[u8::try_from(i + 1).unwrap(); 32]],
            json!({ "allowance_id": format!("d0efffb7-191b-4709-b6c7-{i:012x}") }),
        );
    }
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SORTS_FIRST,
    );
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    wallet.app_mut().receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
}

/// The earlier signed acceptance is parked on its original feed read. When the ordinary
/// retry page is full, a later rejection can enter local storage first in the same millisecond.
async fn ordered_stored_replies(full_retry_page: bool) {
    use pubky_switch_client::app::{FeedPosition, ParkReason};
    let w = World::new();
    let limited = std::sync::Arc::new(crate::common::FailingRecordReads::default());
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        Default::default(),
        limited.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = Box::pin(shop(&w)).await;
    let (_, other, other_id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let (canonical, proposal, id) =
        Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let other_key = pending_key(&wallet, &w.bob_id(), &other_id);
    Box::pin(read_listed_records(&mut wallet)).await;
    wallet.app_mut().flush_cursor().await.unwrap();
    if full_retry_page {
        limited.cap_record(Some(&other_key));
        for i in 0..16u8 {
            answers_with_id(
                &w,
                &wallet,
                "paykit.allowance_acceptance",
                &other,
                &other_id,
                &format!("f0e1d2c3-b4a5-4697-8879-6a5b4c3d2e{i:02x}"),
            );
        }
    }
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_acceptance",
        &proposal,
        &id,
        SENT_FIRST,
    );
    limited.limit_next_record(&physical);
    wallet.app_mut().receive(None).await.unwrap();
    let entries = wallet.app_mut().parked_entries().await.unwrap();
    assert_eq!(entries.len(), if full_retry_page { 17 } else { 1 });
    let mut original = None;
    for entry in entries {
        let parked = wallet
            .app_mut()
            .parked_entry(entry.stored_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.reason, ParkReason::StoreRejected);
        assert!(parked.sender_verified && parked.sender == w.bob_id());
        if parked
            .message
            .as_ref()
            .is_some_and(|msg| msg.msg_id == SENT_FIRST)
        {
            original = Some((
                parked.accepted_at_ms,
                FeedPosition {
                    generation: parked.generation,
                    seq: parked.seq,
                },
            ));
        }
    }
    let (accepted_at, position) = original.expect("the stored original acceptance");
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SORTS_FIRST,
    );
    let received = wallet.app_mut().receive(None).await.unwrap();
    assert!(
        received.events.iter().any(
            |event| matches!(event,AppEvent::MessageReceived {msg_id,..} if msg_id == SORTS_FIRST)
        ),
        "{received:?}"
    );
    if full_retry_page {
        assert!(Box::pin(keeps(&wallet, b"pr/", SORTS_FIRST)).await);
        assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        limited.cap_record(None);
    }
    for _ in 0..4 {
        wallet.receive(None).await.unwrap();
    }
    let kept = wallet.app().protocol_scan("paykit", b"pr/").await.unwrap();
    let (_, bytes) = kept
        .iter()
        .find(|(k, _)| k.ends_with(SENT_FIRST.as_bytes()))
        .expect("the original acceptance stays owed");
    let val: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(val["d"]["accepted_at"], accepted_at);
    assert_eq!(val["d"]["generation"], position.generation);
    assert_eq!(val["d"]["seq"], position.seq);
    assert_eq!(
        wallet.allowance(&canonical).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    assert!(wallet
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        Box::pin(pending_at(&wallet, &canonical))
            .await
            .proposal()
            .hash(),
        proposal.hash()
    );
}

#[tokio::test]
async fn test_allowances_stored_answer_ranks_before_a_later_answer_of_one_millisecond() {
    Box::pin(ordered_stored_replies(false)).await;
}

#[tokio::test]
async fn test_allowances_retried_answer_keeps_the_feed_position_it_was_read_at() {
    Box::pin(ordered_stored_replies(true)).await;
}

#[tokio::test]
async fn test_allowances_a_rejection_after_recreation_closes_only_the_new_proposal_at_the_bound() {
    // A sibling's verified deletion retires the earlier rejection even at the sender limit.
    // After an identical recreation, only the new rejection can close the new lifetime.
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_FIRST,
    );
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    Box::pin(delete_pending_record(&w, &id)).await;
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    for i in 0..paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        send(
            &w,
            &w.bob,
            "shop",
            &w.alice,
            wallet.app_id(),
            "paykit.allowance_rejection",
            vec![[u8::try_from(i + 1).unwrap(); 32]],
            json!({ "allowance_id": format!("d0efffb7-191b-4709-b6c7-{i:012x}") }),
        );
    }
    wallet.receive(None).await.unwrap();
    assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    let again = Box::pin(records_proposal(&mut wallet, &proposal, &id)).await;
    assert_eq!(again, mine);
    let writes = w.record_writes(&w.alice, &physical);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(pending_at(&wallet, &mine)).await.proposal().hash(),
        proposal.hash()
    );
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SORTS_FIRST,
    );
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(Box::pin(held_deleted(&wallet, &physical)).await);
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
}

#[tokio::test]
async fn test_allowances_later_rejection_stops_existing_consent_before_the_conflict_write() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut exec = Box::pin(alice_app(
        &w,
        "exec",
        AppScopes::new()
            .pay()
            .allowance()
            .allowance_execute()
            .build()
            .unwrap(),
    ))
    .await;
    let (pending, p, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut exec)).await;
    assert!(!exec.allowance(&pending).await.unwrap().consented);
    let theirs = shop.allowances().await.unwrap()[0].record;
    let permanent = pending;
    Box::pin(accepted(&w, &mut shop, &theirs, &mut exec, &permanent)).await;
    Box::pin(grant(&w, &mut exec, &permanent)).await;
    exec.receive(None).await.unwrap();
    assert!(exec.allowance(&permanent).await.unwrap().consented);
    let (q, _) = Box::pin(response_and_conflicted(&w, &exec, &permanent)).await;
    assert!(q.is_some());
    answers_with_id(
        &w,
        &exec,
        "paykit.allowance_rejection",
        &p,
        &id,
        SORTS_FIRST,
    );
    fail_writes_with(&w, 1, pubky_switch_core::http::ErrorCode::TooLarge);
    exec.receive(None).await.unwrap();
    assert_eq!(
        exec.allowance(&permanent).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    assert!(Box::pin(keeps(&exec, b"cf/", &b64_of(&permanent))).await);
    assert_eq!(
        Box::pin(response_and_conflicted(&w, &exec, &permanent)).await,
        (q.clone(), None)
    );
    assert_eq!(
        Box::pin(consent_entry(&exec, &permanent)).await,
        Some(json!({"removed": {"reason":"conflicted"}}))
    );
    Box::pin(consent(&mut exec, &permanent)).await;
    assert!(!exec.allowance(&permanent).await.unwrap().consented);
    exec.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(response_and_conflicted(&w, &exec, &permanent)).await,
        (q, Some(SORTS_FIRST.to_owned()))
    );
    assert!(!exec.allowance(&permanent).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_answer_after_a_given_up_earlier_answer_never_takes_the_response() {
    // Bob's acceptance arrives first. The later rejection establishes the conflict.
    // Giving up the acceptance at day 30 keeps the stop in effect for the next rejection.
    const SENT_THIRD: &str = "7b3e9a1c-5d2f-4a8e-b6c4-1e9f3d7a2b58";
    let w = World::new();
    let base = crate::common::store();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        std::sync::Arc::clone(&base),
    ))
    .await;
    let (mine, proposal, id) = Box::pin(recorded_proposal_of(&w, &mut shop, &mut wallet)).await;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_acceptance",
        &proposal,
        &id,
        SENT_FIRST,
    );
    Box::pin(receive_without_allowance_view(&w, &mut wallet, &mine)).await;
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    w.clock.advance_ms(1);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_SECOND,
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(kept_under(&wallet, b"pc/")).await, 1);
    assert!(Box::pin(keeps(&wallet, b"pc/", &b64_of(&physical))).await);
    let stop = wallet.app().protocol_scan("paykit", b"pc/").await.unwrap();
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    wallet.app_mut().rotate().await.unwrap();
    wallet.app_mut().flush_cursor().await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, 0);
    let notices = wallet.notices().await.unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp),
        "{notices:?}"
    );
    assert_eq!(Box::pin(kept_under(&wallet, b"pc/")).await, 1);
    answers_with_id(
        &w,
        &wallet,
        "paykit.allowance_rejection",
        &proposal,
        &id,
        SENT_THIRD,
    );
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet.app().shared_record(&mine).await.unwrap().is_none());
    assert_eq!(Box::pin(kept_under(&wallet, b"pc/")).await, 1);
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"pc/").await.unwrap(),
        stop
    );
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, 0);
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
}

async fn consent_state_keeper(
    w: &World,
) -> (
    std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
    Paykit,
) {
    let store = crate::common::store();
    let mut app = w
        .join_with(
            &w.alice,
            "keeper",
            AppScopes::new()
                .allowance_execute()
                .manage_contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&store),
        )
        .await;
    app.app_mut().invite(&w.bob_id()).await.unwrap();
    (store, app)
}

async fn put_consent_state(app: &mut Paykit, key: &[u8], value: &Value) {
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.to_vec(),
                value: serde_json::to_vec(value).unwrap(),
            }],
        )
        .await
        .unwrap();
}

async fn consent_state_restore_refused(
    w: &World,
    app: &mut Paykit,
    valid: &paykit_sdk::PaykitBackup,
    key: &[u8],
    value: &Value,
) {
    assert!(matches!(
        app.backup().await,
        Err(PaykitError::Client(
            pubky_switch_client::app::AppError::InvalidData(_)
        ))
    ));
    let valid: Value = serde_json::from_slice(&valid.to_bytes().unwrap()).unwrap();
    let mut layer = crate::restore::layer_of(&valid);
    let entry = layer.iter_mut().find(|(found, _)| found == key).unwrap();
    entry.1 = value.clone();
    let backup = paykit_sdk::PaykitBackup::from_bytes(
        &serde_json::to_vec(&crate::restore::with_layer(&valid, layer)).unwrap(),
    )
    .unwrap();
    let store = crate::common::store();
    let result = Box::pin(Paykit::restore(
        w.env(&w.alice, std::sync::Arc::clone(&store)),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await;
    assert!(matches!(
        result,
        Err(PaykitError::Client(
            pubky_switch_client::app::AppError::InvalidData(_)
        ))
    ));
    let ns = pubky_switch_client::store::client_namespace(&app.app_id());
    assert!(store.scan(&ns, b"", None, 1).unwrap().is_empty());
}

async fn removed_consent_unknown_member(nested: bool) {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let (store, mut app) = Box::pin(consent_state_keeper(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut app)).await;
    Box::pin(accept_elsewhere(&w, &mut app, &mine)).await;
    app.end_allowance(&mine).await.unwrap();
    let (key, bytes) = app
        .app()
        .protocol_scan("paykit", b"ac/")
        .await
        .unwrap()
        .remove(0);
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["d"]["consent"],
        json!({"removed": {"reason": "ended"}})
    );
    let backup = app.backup().await.unwrap();
    let valid = Box::pin(restored_unreceived(&w, &backup)).await;
    assert_eq!(
        valid.app().protocol_get("paykit", &key).await.unwrap(),
        Some(bytes)
    );
    drop(valid);
    if nested {
        value["d"]["consent"]["removed"]["unexpected"] = json!(true);
    } else {
        value["d"]["unexpected"] = json!(true);
    }
    Box::pin(put_consent_state(&mut app, &key, &value)).await;
    let id = app.app_id();
    drop(app);
    let mut app = w
        .load_on_clock(&w.alice, store, id, std::sync::Arc::clone(&w.clock))
        .await;
    assert_eq!(
        app.app().protocol_get("paykit", &key).await.unwrap(),
        Some(serde_json::to_vec(&value).unwrap())
    );
    let error = app.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
    Box::pin(consent_state_restore_refused(
        &w, &mut app, &backup, &key, &value,
    ))
    .await;
}

#[tokio::test]
async fn test_consent_state_removed_entry_unknown_member_is_lazy_and_refused() {
    Box::pin(removed_consent_unknown_member(false)).await;
}

#[tokio::test]
async fn test_consent_state_removed_branch_unknown_member_is_lazy_and_refused() {
    Box::pin(removed_consent_unknown_member(true)).await;
}

#[tokio::test]
async fn test_consent_state_supplied_held_values_restore_and_unknown_member_refuses() {
    // `executed` supplies the signed grant and initial consent; it does not grant through
    // an executor API. Both current Held values must survive the body reader.
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    let key = format!("ac/{}", pubky_switch_core::enc::b64(&mine)).into_bytes();
    let bytes = exec
        .app()
        .protocol_get("paykit", &key)
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    let backup = exec.backup().await.unwrap();
    for confirmed in [false, true] {
        value["d"]["consent"]["held"]["confirm_again"] = json!(confirmed);
        Box::pin(put_consent_state(&mut exec, &key, &value)).await;
        exec.receive(None).await.unwrap();
        let restored = Box::pin(restored_unreceived(&w, &exec.backup().await.unwrap())).await;
        assert_eq!(
            Box::pin(consent_entry(&restored, &mine)).await,
            Some(json!({"held": {"confirm_again": true}}))
        );
    }
    value["d"]["consent"]["held"]["unexpected"] = json!(true);
    Box::pin(put_consent_state(&mut exec, &key, &value)).await;
    let error = exec.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
    Box::pin(consent_state_restore_refused(
        &w, &mut exec, &backup, &key, &value,
    ))
    .await;
}

#[tokio::test]
async fn test_consent_state_supplied_grant_notice_routes_and_unknown_member_refuses() {
    // The grant is supplied by `executed`; ending the Allowance produces the target.
    let w = World::new();
    let (_shop, mut wallet, exec, _phone, _, mine) = Box::pin(executed(&w)).await;
    wallet.end_allowance(&mine).await.unwrap();
    let (key, bytes) = wallet
        .app()
        .protocol_scan("paykit", b"en/")
        .await
        .unwrap()
        .remove(0);
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    let backup = wallet.backup().await.unwrap();
    let restored = Box::pin(restored_unreceived(&w, &backup)).await;
    assert_eq!(
        restored.app().protocol_get("paykit", &key).await.unwrap(),
        Some(bytes.clone())
    );
    drop(restored);
    // The client routing call takes the decoded fields of the notice queued above.
    let message = pubky_switch_core::message::Message {
        kind: "paykit.executor_notice".into(),
        msg_id: value["d"]["msg_id"].as_str().unwrap().to_owned(),
        created_at_ms: w.clock.now_ms(),
        from: pubky_switch_core::message::Sender {
            identity: w.alice_id(),
            app_id: wallet.app_id(),
        },
        to: w.alice_id(),
        refs: vec![],
        body: json!({"allowance_record": pubky_switch_core::enc::b64(&mine), "reason": "ended"})
            .as_object()
            .unwrap()
            .clone(),
    };
    assert_eq!(
        wallet
            .app_mut()
            .recipients_for(&w.alice_id(), &message)
            .await
            .unwrap(),
        vec![exec.app_id()]
    );
    value["d"]["unexpected"] = json!(true);
    Box::pin(put_consent_state(&mut wallet, &key, &value)).await;
    let error = wallet
        .app_mut()
        .recipients_for(&w.alice_id(), &message)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            pubky_switch_client::app::AppError::Refused(pubky_switch_client::app::Refusal::NotNow(
                _
            ))
        ),
        "{error:?}"
    );
    Box::pin(consent_state_restore_refused(
        &w,
        &mut wallet,
        &backup,
        &key,
        &value,
    ))
    .await;
    let error = wallet.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
    wallet
        .app_mut()
        .protocol_commit("paykit", &[LocalWrite::Put { key, value: bytes }])
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"en/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_consent_state_real_owed_end_unknown_member_is_lazy_and_refused() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let (store, mut app) = Box::pin(consent_state_keeper(&w)).await;
    let (_, mine) = Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut app,
        SendsHeld::Blocked,
    ))
    .await;
    assert!(Box::pin(ends_naming_both(&app, &mine)).await);
    let (key, bytes) = app
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .remove(0);
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    app.receive(None).await.unwrap();
    assert_eq!(
        app.app().protocol_get("paykit", &key).await.unwrap(),
        Some(bytes)
    );
    value["d"]["unexpected"] = json!(true);
    Box::pin(put_consent_state(&mut app, &key, &value)).await;
    let id = app.app_id();
    drop(app);
    let mut app = w
        .load_on_clock(&w.alice, store, id, std::sync::Arc::clone(&w.clock))
        .await;
    assert_eq!(
        app.app().protocol_get("paykit", &key).await.unwrap(),
        Some(serde_json::to_vec(&value).unwrap())
    );
    let error = app.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
    assert_eq!(
        app.app().protocol_get("paykit", &key).await.unwrap(),
        Some(serde_json::to_vec(&value).unwrap())
    );
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

#[tokio::test]
async fn test_consent_state_owed_end_body_discarded_restore_recovers_signed_end() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut app = Box::pin(keeper(&w)).await;
    let (_, mine) = Box::pin(crossing_while_held(
        &w,
        &mut shop,
        &mut app,
        SendsHeld::Blocked,
    ))
    .await;
    assert!(Box::pin(ends_naming_both(&app, &mine)).await);
    let (key, bytes) = app
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .remove(0);
    let owed: Value = serde_json::from_slice(&bytes).unwrap();
    let original_id = owed["d"]["msg_id"].as_str().unwrap().to_owned();
    let original = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&app, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap()
    .end
    .unwrap();
    assert_eq!(original.message().msg_id, original_id);
    // This body is intentionally not read at restore; the valid family key is counted.
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key,
                value: b"unread body".to_vec(),
            }],
        )
        .await
        .unwrap();
    let backup = app.backup().await.unwrap();
    drop(app);
    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    assert!(restored
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
    restored.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    restored.unblock(&w.bob_id()).await.unwrap();
    restored.receive(None).await.unwrap();
    assert!(Box::pin(ends_naming_both(&restored, &mine)).await);
    let summary = shop.receive(None).await.unwrap();
    let delivered: Vec<_> = summary
        .report
        .events
        .iter()
        .filter_map(|event| match event {
            AppEvent::MessageReceived { kind, msg_id, .. } if kind == "paykit.allowance_end" => {
                Some(msg_id.clone())
            }
            _ => None,
        })
        .collect();
    assert!(delivered.is_empty());
    assert!(crate::common::possibly_delivered(&restored, &original_id).await);
    let recovered = AllowanceRecord::read(
        &serde_json::to_vec(&Box::pin(value(&restored, &mine)).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap()
    .end
    .unwrap();
    assert_eq!(recovered.hash(), original.hash());
    assert_eq!(recovered.plaintext(), original.plaintext());
    restored.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

/// A proposal signed and stored by its sender, as the sender's record would hold it.
fn signed_pending_proposal(
    app: &Paykit,
    to: [u8; 32],
    id: &paykit_lib::AllowanceId,
    proposer_role: &str,
) -> StoredMessage {
    let body = json!({
        "allowance_id": id.as_str(),
        "proposer_role": proposer_role,
        "allower_generation": 1,
        "terms": terms().to_json().unwrap()
    });
    let signed = app
        .app()
        .sign_message(OutgoingMessage::new(
            to,
            "paykit.allowance_proposal",
            body.as_object().unwrap().clone(),
        ))
        .unwrap();
    StoredMessage::new(
        app.identity(),
        app.app().cert().signed(),
        signed.plaintext().clone(),
        None,
        &to,
    )
    .unwrap()
}

#[tokio::test]
async fn test_pending_proposal_conflict_is_visible_before_placement_and_discarded_at_the_canonical_key(
) {
    Box::pin(place_competing_proposal(PendingChange::Unchanged)).await;
}

#[tokio::test]
async fn test_pending_proposal_conflict_places_the_second_proposal_after_a_live_rewrite() {
    Box::pin(place_competing_proposal(PendingChange::Rewritten)).await;
}

#[tokio::test]
async fn test_pending_proposal_conflict_keeps_an_identical_recreation_separate() {
    Box::pin(place_competing_proposal(PendingChange::Recreated)).await;
}

#[tokio::test]
async fn test_pending_proposal_conflict_preserves_a_live_successors_unknown_member() {
    Box::pin(place_competing_proposal(PendingChange::Extended)).await;
}

enum PendingChange {
    Unchanged,
    Rewritten,
    Extended,
    Recreated,
}

async fn place_competing_proposal(change: PendingChange) {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, RecordTarget};

    let w = World::new();
    let mut wallet = Box::pin(wallet(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let p = signed_pending_proposal(&wallet, w.bob_id(), &id, "allower");
    let other = signed_pending_proposal(&shop, w.alice_id(), &id, "allowee");
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    let permanent = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &id))
        .unwrap();
    let physical = wallet.app().record_key(&target).unwrap();
    let original = PendingAllowanceRecord::new(&w.alice_id(), p.clone())
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut phone, &target, Some(&original))).await;
    for app in [&mut wallet, &mut phone] {
        Box::pin(read_listed_records(app)).await;
        let view = app.allowance(&permanent).await.unwrap();
        assert_eq!(view.status, AllowanceStatus::Proposed);
        assert_eq!(view.record, permanent);
        assert_eq!(view.counterparty, w.bob_id());
        assert!(app.app().shared_record(&permanent).await.unwrap().is_none());
    }
    Box::pin(records_proposal(&mut shop, &other, &id)).await;
    let msg_id = shop
        .app_mut()
        .queue_stored(other.plaintext())
        .await
        .unwrap();
    assert_eq!(msg_id, other.message().msg_id);
    shop.flush().await.unwrap();
    for app in [&mut wallet, &mut phone] {
        let accepted = app.app_mut().receive(None).await.unwrap();
        assert!(accepted
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageReceived{msg_id:id,..} if *id==msg_id)));
        let view = app.allowance(&permanent).await.unwrap();
        assert_eq!(view.record, permanent);
        assert_eq!(view.status, AllowanceStatus::Conflicted);
        assert_eq!(view.counterparty, w.bob_id());
        assert_eq!(app.allowances().await.unwrap().len(), 1);
        assert!(matches!(
            app.allowance(&physical).await.unwrap_err().refusal(),
            Some(Refusal::NotFound)
        ));
        assert_eq!(
            app.app()
                .shared_record(&physical)
                .await
                .unwrap()
                .unwrap()
                .content
                .value(),
            Some(original.as_slice())
        );
    }
    let first = wallet
        .app()
        .shared_record(&physical)
        .await
        .unwrap()
        .unwrap()
        .version;
    match change {
        PendingChange::Unchanged => (),
        PendingChange::Rewritten | PendingChange::Extended => {
            let current = if matches!(change, PendingChange::Extended) {
                let mut value: Value = serde_json::from_slice(&original).unwrap();
                value["shop_display"] = json!({"currency":"BTC"});
                serde_json::to_vec(&value).unwrap()
            } else {
                original.clone()
            };
            Box::pin(write_record(&mut phone, &target, Some(&current))).await;
            let saved = phone.app().shared_record(&physical).await.unwrap().unwrap();
            assert!(saved.version > first);
            assert_eq!(saved.content.value(), Some(current.as_slice()));
        }
        PendingChange::Recreated => {
            Box::pin(write_record(&mut phone, &target, None)).await;
            Box::pin(write_record(&mut phone, &target, Some(&original))).await;
            for _ in 0..3 {
                wallet.receive(None).await.unwrap();
            }
            let reopened = wallet
                .app()
                .shared_record(&physical)
                .await
                .unwrap()
                .unwrap();
            assert!(reopened.version > first);
            assert!(
                reopened.authenticated
                    && !reopened.doubtful
                    && !reopened.flagged
                    && !reopened.divergent
            );
            assert_eq!(reopened.content.value(), Some(original.as_slice()));
            assert!(wallet
                .app()
                .shared_record(&permanent)
                .await
                .unwrap()
                .is_none());
            assert!(w.rail.all_executions().is_empty());
            return;
        }
    }
    for app in [&mut wallet, &mut phone] {
        app.receive(None).await.unwrap();
        let held = app.app().shared_record(&physical).await.unwrap().unwrap();
        let placed =
            PendingAllowanceRecord::read(held.content.value().unwrap(), &w.alice_id()).unwrap();
        if matches!(change, PendingChange::Extended) {
            let raw: Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
            assert_eq!(raw["shop_display"], json!({"currency":"BTC"}));
        }
        assert_eq!(placed.proposal().hash(), p.hash());
        assert_eq!(placed.conflicted().unwrap().hash(), other.hash());
    }
    assert!(phone.discard_pending_allowance(&physical).await.is_err());
    phone.discard_pending_allowance(&permanent).await.unwrap();
    wallet.receive(None).await.unwrap();
    for app in [&wallet, &phone] {
        assert!(app.app().record_seen(&physical).await.unwrap().is_none());
        assert!(app.allowances().await.unwrap().is_empty());
        assert!(app.app().records_of("allowances").await.unwrap().is_empty());
        assert!(app
            .app()
            .records_of("allowance_ledgers")
            .await
            .unwrap()
            .is_empty());
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_proposal_conflict_restore_preserves_the_first_proposal_and_owed_acceptance() {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, RecordTarget};
    use std::sync::Arc;
    let w = World::new();
    let store: Arc<dyn pubky_switch_client::store::StateStore> =
        Arc::new(pubky_switch_client::store::MemoryStore::new());
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        Default::default(),
        store.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = Box::pin(shop(&w)).await;
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let p = signed_pending_proposal(&wallet, w.bob_id(), &id, "allower");
    let p2 = signed_pending_proposal(&shop, w.alice_id(), &id, "allowee");
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let canonical = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &id))
        .unwrap();
    let original = PendingAllowanceRecord::new(&w.alice_id(), p.clone())
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut wallet, &target, Some(&original))).await;
    Box::pin(read_listed_records(&mut wallet)).await;
    // Each original is recorded on its own account before either app reads the other's.
    Box::pin(records_proposal(&mut shop, &p2, &id)).await;
    let original_id = wallet.app_mut().queue_stored(p.plaintext()).await.unwrap();
    wallet.flush().await.unwrap();
    let before = wallet.backup().await.unwrap();
    shop.app_mut().queue_stored(p2.plaintext()).await.unwrap();
    shop.flush().await.unwrap();
    let received = shop.receive(None).await.unwrap();
    assert!(received
        .report
        .events
        .iter()
        .any(|e| matches!(e,AppEvent::MessageReceived{msg_id,..} if msg_id==&original_id)));
    inject_with_id(
        &w,
        &w.bob,
        "shop",
        &[wallet.app_id()],
        "paykit.allowance_acceptance",
        vec![p.hash()],
        json!({"allowance_id":id.as_str()}),
        SENT_FIRST,
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&canonical).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    let owed = wallet.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(owed.len(), 1);
    assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    drop(wallet);
    let mut restored = Box::pin(restore_pending_wallet(&w, store.clone(), &before)).await;
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap(),
        owed
    );
    Box::pin(read_listed_records(&mut restored)).await;
    let held = restored.app().shared_record(&key).await.unwrap().unwrap();
    let conflict =
        PendingAllowanceRecord::read(held.content.value().unwrap(), &w.alice_id()).unwrap();
    assert_eq!(conflict.proposal().hash(), p.hash());
    assert_eq!(conflict.conflicted().unwrap().hash(), p2.hash());
    assert_eq!(
        restored.allowance(&canonical).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    restored
        .discard_pending_allowance(&canonical)
        .await
        .unwrap();
    assert!(!Box::pin(keeps(&restored, b"pr/", SENT_FIRST)).await);
    assert!(restored
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
    drop(restored);
    let mut restored = Box::pin(restore_pending_wallet(&w, store, &before)).await;
    let deletion = restored.app_mut().read_record_fresh(&target).await.unwrap();
    assert!(
        matches!(deletion,pubky_switch_client::app::RecordRead::Fresh(ref held) if held.authenticated && !held.not_taken && !held.doubtful && held.content==pubky_switch_client::app::RecordContent::Deleted)
    );
    for _ in 0..4 {
        restored.receive(None).await.unwrap();
    }
    let current = restored.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        current.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    assert!(restored
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
    assert!(restored
        .app()
        .protocol_scan("paykit", b"pc/")
        .await
        .unwrap()
        .is_empty());
    assert!(!Box::pin(keeps(&restored, b"pr/", SENT_FIRST)).await);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_proposal_conflict_retries_a_stored_proposal_at_the_existing_pending_key() {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, RecordTarget};
    let w = World::new();
    let limited = std::sync::Arc::new(crate::common::FailingRecordReads::default());
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        Default::default(),
        limited.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let other = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    let signed = Box::pin(pending_at(&shop, &other.record))
        .await
        .proposal()
        .clone();
    let p = signed_pending_proposal(&phone, w.bob_id(), &other.allowance_id, "allower");
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &other.allowance_id),
    );
    let physical = wallet.app().record_key(&target).unwrap();
    let canonical = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &other.allowance_id))
        .unwrap();
    let original = PendingAllowanceRecord::new(&w.alice_id(), p.clone())
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut phone, &target, Some(&original))).await;
    Box::pin(read_listed_records(&mut wallet)).await;
    assert_eq!(Box::pin(raw(&wallet, &physical)).await, original);
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    shop.flush().await.unwrap();
    limited.limit_next_record(&physical);
    let stopped = wallet.receive(None).await.unwrap();
    let entries = wallet.app_mut().parked_entries().await.unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.hash == signed.hash())
        .expect("the incoming signed proposal is held at the failed read");
    assert_eq!(
        entry.reason,
        pubky_switch_client::app::ParkReason::StoreRejected
    );
    let kept = wallet
        .app_mut()
        .parked_entry(entry.stored_id)
        .await
        .unwrap()
        .unwrap();
    assert!(kept.sender_verified);
    assert_eq!(kept.message.unwrap().msg_id, other.msg_id);
    assert!(stored_locally(&stopped));
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let held = wallet
        .app()
        .shared_record(&physical)
        .await
        .unwrap()
        .unwrap();
    let placed =
        PendingAllowanceRecord::read(held.content.value().unwrap(), &w.alice_id()).unwrap();
    assert_eq!(placed.proposal().hash(), p.hash());
    assert_eq!(placed.conflicted().unwrap().hash(), signed.hash());
    assert!(!Box::pin(keeps(&wallet, b"se/", &other.msg_id)).await);
    assert_eq!(
        wallet.allowance(&canonical).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    assert!(wallet
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
}

/// Two original wallets sign proposals and a sibling holds their account's pending record.
/// The user discards a blocked incoming or an already conflicted write without sending an end.
#[tokio::test]
async fn test_pending_proposal_conflict_discard_checks_the_selected_record_and_sends_nothing() {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, RecordContent, RecordTarget};

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new()
            .pay()
            .allowance()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let own = w.alice_id();
    let peer = w.bob_id();
    let incoming = signed_pending_proposal(&shop, own, &id, "allowee");
    let outgoing = signed_pending_proposal(&phone, peer, &id, "allower");
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&peer, &id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let canonical = wallet
        .app()
        .record_key(&allowances_target(&peer, &id))
        .unwrap();
    let mut original = PendingAllowanceRecord::new(&own, incoming.clone()).unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&original.to_json().unwrap()),
    ))
    .await;
    Box::pin(read_listed_records(&mut wallet)).await;
    assert!(matches!(
        wallet
            .discard_pending_allowance(&canonical)
            .await
            .unwrap_err()
            .refusal(),
        Some(Refusal::Usage(_))
    ));
    let first = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        first.content.value(),
        Some(original.to_json().unwrap().as_slice())
    );

    // This independent write is newer than the user's selection. The first call only
    // learns of it. The user may then choose the now-conflicted proposal.
    original.note_proposal(outgoing.clone()).unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&original.to_json().unwrap()),
    ))
    .await;
    assert!(matches!(
        wallet
            .discard_pending_allowance(&canonical)
            .await
            .unwrap_err()
            .refusal(),
        Some(Refusal::NotNow(_))
    ));
    let current = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_ne!(first.version, current.version);
    assert_eq!(
        current.content.value(),
        Some(original.to_json().unwrap().as_slice())
    );
    wallet.discard_pending_allowance(&canonical).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );

    phone.receive(None).await.unwrap();
    let next = PendingAllowanceRecord::new(&own, incoming).unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&next.to_json().unwrap()),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    phone.block(&peer).await.unwrap();
    wallet.discard_pending_allowance(&canonical).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );

    // A block allows discard only when the counterparty sent this original.
    phone.receive(None).await.unwrap();
    let last = PendingAllowanceRecord::new(&own, outgoing).unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&last.to_json().unwrap()),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    assert!(matches!(
        wallet
            .discard_pending_allowance(&canonical)
            .await
            .unwrap_err()
            .refusal(),
        Some(Refusal::Usage(_))
    ));
    assert_eq!(
        wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .content
            .value(),
        Some(last.to_json().unwrap().as_slice())
    );
    wallet.flush().await.unwrap();
    let summary = shop.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.allowance_rejection"));
    assert!(!received(&summary, "paykit.allowance_end"));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_proposal_conflict_discard_reads_the_current_proposal_after_restore() {
    Box::pin(discard_after_restore(true)).await;
}

#[tokio::test]
async fn test_pending_proposal_conflict_discard_waits_while_the_restore_feed_is_unavailable() {
    Box::pin(discard_after_restore(false)).await;
}

async fn discard_after_restore(feed_available: bool) {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, RecordContent, RecordTarget};

    let w = World::new();
    let mut wallet = Box::pin(wallet(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);

    let id = paykit_lib::AllowanceId::generate().unwrap();
    let peer = w.bob_id();
    let incoming = signed_pending_proposal(&shop, w.alice_id(), &id, "allowee");
    let outgoing = signed_pending_proposal(&phone, peer, &id, "allower");
    let mut pending = PendingAllowanceRecord::new(&w.alice_id(), incoming).unwrap();
    pending.note_proposal(outgoing).unwrap();
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&peer, &id),
    );
    let key = phone.app().record_key(&target).unwrap();
    let canonical = phone
        .app()
        .record_key(&allowances_target(&peer, &id))
        .unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&pending.to_json().unwrap()),
    ))
    .await;

    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    if !feed_available {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/feed".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: Value::Null,
            });
        assert!(restored
            .discard_pending_allowance(&canonical)
            .await
            .is_err());
        assert!(restored.app().restore_held_cursor().is_none());
        let current = phone.app_mut().read_record_fresh(&target).await.unwrap();
        assert!(
            matches!(current, pubky_switch_client::app::RecordRead::Fresh(ref held)
            if held.authenticated && held.content.value()==Some(pending.to_json().unwrap().as_slice()))
        );
        return;
    }
    restored
        .discard_pending_allowance(&canonical)
        .await
        .unwrap();
    let deleted = restored.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(deleted.content, RecordContent::Deleted);
    assert!(restored
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
    restored.flush().await.unwrap();
    let seen = shop.receive(None).await.unwrap();
    assert!(!received(&seen, "paykit.allowance_rejection"));
    assert!(!received(&seen, "paykit.allowance_end"));
}

#[tokio::test]
async fn test_pending_record_keeps_the_first_proposal_and_conflict_until_its_verified_deletion() {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, RecordContent, RecordTarget};
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let incoming = signed_pending_proposal(&shop, w.alice_id(), &id, "allowee");
    let outgoing = signed_pending_proposal(&phone, w.bob_id(), &id, "allower");
    let collection = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(collection.name(), collection.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let own = w.alice_id();
    let mut record = PendingAllowanceRecord::new(&own, incoming.clone()).unwrap();
    let first_value = record.to_json().unwrap();
    Box::pin(write_record(&mut phone, &target, Some(&first_value))).await;
    let first = wallet.receive(None).await.unwrap();
    assert!(first
        .report
        .events
        .iter()
        .any(|event| matches!(event, AppEvent::RecordChanged { key: changed } if changed == &key)));
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let read = PendingAllowanceRecord::read(held.content.value().unwrap(), &own).unwrap();
    assert_eq!(read.proposal().hash(), incoming.hash());
    assert_eq!(read.counterparty(), &w.bob_id());
    assert_eq!(read.allowance_id(), &id);
    let canonical = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &id))
        .unwrap();
    let views = wallet.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].record, canonical);
    assert_eq!(views[0].status, AllowanceStatus::Proposed);
    assert!(wallet
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
    let original_mark = wallet.app().record_seen(&key).await.unwrap().unwrap();
    assert_eq!(
        phone.app().record_seen(&key).await.unwrap().as_ref(),
        Some(&original_mark)
    );

    assert!(record.note_proposal(outgoing.clone()).unwrap());
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&record.to_json().unwrap()),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    let mark = wallet.app().record_seen(&key).await.unwrap().unwrap();
    assert_ne!(original_mark, mark);
    assert_eq!(
        phone.app().record_seen(&key).await.unwrap().as_ref(),
        Some(&mark)
    );

    // The writer has seen both roles. Its older copy is refused before signing.
    let before = phone.app().shared_record(&key).await.unwrap().unwrap();
    let refused = phone
        .app_mut()
        .record_op(&target, Some(&first_value), Vec::new())
        .await
        .unwrap_err();
    assert!(refused
        .to_string()
        .contains("does not preserve the roles at this key"));
    let current = phone.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(current.version, before.version);
    assert_eq!(current.content, before.content);
    assert_eq!(wallet.app().record_seen(&key).await.unwrap().unwrap(), mark);
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&record.to_json().unwrap()),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    assert!(
        !wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .flagged
    );

    // A network interruption does not end the lifetime of either signed role.
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/records/".into(),
        after: 0,
    });
    let uncertain = wallet.app_mut().read_record_fresh(&target).await.unwrap();
    assert!(matches!(
        uncertain,
        pubky_switch_client::app::RecordRead::Kept {
            reason: pubky_switch_client::app::KeptReason::Unreachable(_),
            ..
        }
    ));
    assert_eq!(wallet.app().record_seen(&key).await.unwrap().unwrap(), mark);

    // Only the writer receives this epoch's key. Its later write is genuine but
    // the wallet must retain the conflict while it cannot open the new value.
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&record.to_json().unwrap()),
    ))
    .await;
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    let epoch = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &wallet.app().cert().clone(), epoch);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(matches!(held.content, RecordContent::Unreadable(_)));
    assert_eq!(wallet.app().record_seen(&key).await.unwrap().unwrap(), mark);

    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    Box::pin(write_record(&mut phone, &target, None)).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().record_seen(&key).await.unwrap().is_none());
    assert!(wallet.allowances().await.unwrap().is_empty());
    let next = PendingAllowanceRecord::new(&own, outgoing.clone()).unwrap();
    Box::pin(write_record(
        &mut phone,
        &target,
        Some(&next.to_json().unwrap()),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    let reopened_mark = wallet.app().record_seen(&key).await.unwrap().unwrap();
    assert_ne!(reopened_mark, original_mark);
    assert_ne!(reopened_mark, mark);
    assert_eq!(
        phone.app().record_seen(&key).await.unwrap().as_ref(),
        Some(&reopened_mark)
    );
    let reopened = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let read = PendingAllowanceRecord::read(reopened.content.value().unwrap(), &own).unwrap();
    assert_eq!(read.proposal().hash(), outgoing.hash());
    assert!(read.conflicted().is_none());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_proposals_remain_bound_to_their_own_records_after_a_restart() {
    use paykit_lib::{derive::natural_key, records::PendingAllowanceRecord, AllowanceId};
    use pubky_switch_client::app::{Collection, RecordTarget};

    let w = World::new();
    let shop = Box::pin(shop(&w)).await;
    let storage = crate::common::store();
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().allowance().build().unwrap(),
        Default::default(),
        storage.clone(),
    ))
    .await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let own = w.alice_id();
    let peer = w.bob_id();
    let first_id = AllowanceId::generate().unwrap();
    let second_id = AllowanceId::generate().unwrap();
    let first = signed_pending_proposal(&shop, own, &first_id, "allowee");
    let second = signed_pending_proposal(&phone, peer, &second_id, "allower");
    let collection = paykit_lib::Collection::AllowanceProposals;
    let target = |id| {
        RecordTarget::new(
            Collection::new(collection.name(), collection.fixed_policy()),
            &natural_key::allowance(&peer, id),
        )
    };
    let first_target = target(&first_id);
    let second_target = target(&second_id);
    let first_key = wallet.app().record_key(&first_target).unwrap();
    let second_key = wallet.app().record_key(&second_target).unwrap();
    assert_ne!(first_key, second_key);
    let first_value = PendingAllowanceRecord::new(&own, first.clone())
        .unwrap()
        .to_json()
        .unwrap();
    let second_value = PendingAllowanceRecord::new(&own, second.clone())
        .unwrap()
        .to_json()
        .unwrap();
    // Both values are signed for this wallet. Selecting another Allowance's target
    // cannot establish a mark or prepare a record operation there.
    let refused = wallet
        .app_mut()
        .record_op(&first_target, Some(&second_value), Vec::new())
        .await
        .unwrap_err();
    assert!(refused
        .to_string()
        .contains("signed proposal belongs to another record"));
    assert!(wallet
        .app()
        .record_seen(&first_key)
        .await
        .unwrap()
        .is_none());
    Box::pin(write_record(&mut phone, &first_target, Some(&first_value))).await;
    Box::pin(write_record(
        &mut wallet,
        &second_target,
        Some(&second_value),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    let marks = [
        wallet.app().record_seen(&first_key).await.unwrap().unwrap(),
        wallet
            .app()
            .record_seen(&second_key)
            .await
            .unwrap()
            .unwrap(),
    ];
    assert_ne!(marks[0], marks[1]);
    let app = wallet.app_id();
    drop(wallet);
    let wallet = Box::pin(w.load_on_clock(&w.alice, storage, app, w.clock.clone())).await;
    for ((key, message), mark) in [(first_key, first), (second_key, second)]
        .into_iter()
        .zip(marks)
    {
        let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
        assert!(held.authenticated && !held.flagged && !held.not_taken);
        let qualified = PendingAllowanceRecord::read(held.content.value().unwrap(), &own).unwrap();
        assert_eq!(qualified.proposal().hash(), message.hash());
        assert_eq!(wallet.app().record_seen(&key).await.unwrap(), Some(mark));
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowances_receive_only_app_can_read_a_proposal_and_cannot_send_or_answer() {
    let w = World::new();
    let mut viewer = Box::pin(alice_app(
        &w,
        "viewer",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let mut author = Box::pin(wallet(&w)).await;
    let refused = viewer
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::ScopeMissing(scope)) if scope == "paykit_contacts")
    );
    assert!(viewer
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
    assert!(viewer
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());

    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    let summary = viewer.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.allowance_proposal"));
    let incoming = viewer
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.allowance_id == sent.allowance_id)
        .unwrap();
    assert_eq!(incoming.status, AllowanceStatus::Proposed);
    let refused = viewer.reject_allowance(&incoming.record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::ScopeMissing(scope)) if scope == "paykit_contacts")
    );
    assert_eq!(
        viewer.allowance(&incoming.record).await.unwrap().status,
        AllowanceStatus::Proposed
    );
    assert!(viewer
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());

    author.receive(None).await.unwrap();
    let rejected = author.reject_allowance(&incoming.record).await.unwrap();
    assert!(matches!(
        author.app().delivery(&rejected).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    shop.receive(None).await.unwrap();
    assert!(matches!(
        shop.allowance(&sent.record).await.unwrap_err().refusal(),
        Some(Refusal::NotFound)
    ));
    let pending = pending_key(&shop, &w.alice_id(), &sent.allowance_id);
    assert!(Box::pin(held_deleted(&shop, &pending)).await);
    assert!(shop
        .app()
        .shared_record(&sent.record)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_allowances_queued_rejection_waits_after_the_signer_loses_contacts() {
    Box::pin(queued_allowance_after_renewal(
        paykit_lib::Kind::AllowanceRejection,
        AppScopes::new().allowance().build().unwrap(),
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_allowances_retry_checks_the_signers_current_roles_for_its_message() {
    use paykit_lib::Kind::{AllowanceEnd, AllowanceProposal, AllowanceRejection};

    for (kind, scopes, delivered) in [
        (AllowanceProposal, AppScopes::new().allowance(), false),
        (AllowanceProposal, AppScopes::new().contacts(), false),
        (AllowanceRejection, AppScopes::new().contacts(), false),
        (AllowanceEnd, AppScopes::new().allowance(), true),
        (AllowanceEnd, AppScopes::new().contacts(), false),
    ] {
        Box::pin(queued_allowance_after_renewal(
            kind,
            scopes.build().unwrap(),
            delivered,
        ))
        .await;
    }
}

async fn queued_allowance_after_renewal(
    kind: paykit_lib::Kind,
    granted: RequestedScopes,
    delivered: bool,
) {
    use paykit_lib::Kind::{AllowanceEnd, AllowanceProposal, AllowanceRejection};

    let w = World::new();
    let mut payer = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let pending = match kind {
        AllowanceProposal => None,
        AllowanceRejection => Some(Box::pin(proposed(&w, &mut shop, &mut payer)).await.1),
        AllowanceEnd => {
            let original = payer
                .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
                .await
                .unwrap();
            let received_proposal = shop.receive(None).await.unwrap();
            assert!(received(&received_proposal, AllowanceProposal.as_str()));
            Some(original.record)
        }
        _ => unreachable!(),
    };
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/inbox".into(),
            after: 0,
        });
    let msg_id = match (kind, pending) {
        (AllowanceProposal, None) => {
            payer
                .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
                .await
                .unwrap()
                .msg_id
        }
        (AllowanceRejection, Some(key)) => payer.reject_allowance(&key).await.unwrap(),
        (AllowanceEnd, Some(key)) => payer.end_allowance(&key).await.unwrap(),
        _ => unreachable!(),
    };
    assert!(matches!(
        payer.app().delivery(&msg_id).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Queued { .. })
    ));
    assert!(payer
        .app_mut()
        .hold_message("paykit", &msg_id)
        .await
        .unwrap());
    let original = payer.app().cert().clone();
    let id = payer.app_id();
    let narrower = renew(&w, id, &granted, 1000);
    list(&w, id, &original, &narrower);
    payer.receive(None).await.unwrap();
    assert_eq!(payer.app().cert().hash(), narrower.hash());
    payer
        .app_mut()
        .release_message("paykit", &msg_id)
        .await
        .unwrap();
    let retried = payer.app_mut().flush_message(&msg_id).await.unwrap();
    assert_eq!(
        matches!(
            retried,
            Some(pubky_switch_client::app::Delivery::Accepted { .. })
        ),
        delivered,
        "{kind:?}",
    );
    let summary = shop.receive(None).await.unwrap();
    assert_eq!(received(&summary, kind.as_str()), delivered, "{kind:?}");
    if kind == AllowanceRejection {
        assert_eq!(
            shop.allowances().await.unwrap()[0].status,
            AllowanceStatus::Proposed
        );
    }
}

async fn signed_recorded_end(app: &Paykit, record: &[u8; 32]) -> StoredMessage {
    let raw = serde_json::to_vec(&Box::pin(value(app, record)).await).unwrap();
    let end = AllowanceRecord::read(&raw, &app.identity())
        .unwrap()
        .end
        .unwrap();
    assert_eq!(end.sender(), app.identity());
    assert_eq!(end.cert().app_id(), app.app_id());
    end
}

/// The Allowance role alone can still stop a payment locally. Only its automatic
/// message to the counterparty waits for a permitting contact read (Paykit spec 3.1, 8.3).
#[tokio::test]
async fn test_allowances_crossing_end_waits_for_contacts_after_renewal_and_reload() {
    let w = World::new();
    let store = crate::common::store();
    let full = AppScopes::new()
        .allowance()
        .manage_contacts()
        .build()
        .unwrap();
    let mut payer = w
        .join_with(
            &w.alice,
            "wallet",
            full.clone(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&store),
        )
        .await;
    payer.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = Box::pin(shop(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut payer)).await;
    Box::pin(own_acceptance_recorded(&mut payer, &mine)).await;
    let app_id = payer.app_id();
    let original = payer.app().cert().clone();
    let narrow = renew(
        &w,
        app_id,
        &AppScopes::new().allowance().build().unwrap(),
        1000,
    );
    list(&w, app_id, &original, &narrow);
    payer.receive(None).await.unwrap();
    assert_eq!(payer.app().cert().hash(), narrow.hash());
    shop.end_allowance(&theirs).await.unwrap();
    payer.receive(None).await.unwrap();

    assert!(Box::pin(ends_naming_both(&payer, &mine)).await);
    let ended = Box::pin(signed_recorded_end(&payer, &mine)).await;
    assert_eq!(
        payer
            .app()
            .protocol_scan("paykit", b"eo/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(payer
        .app()
        .delivery(&ended.message().msg_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    drop(payer);
    let mut payer = w
        .load_on_clock(&w.alice, store, app_id, std::sync::Arc::clone(&w.clock))
        .await;
    payer.receive(None).await.unwrap();
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    assert!(payer
        .app()
        .delivery(&ended.message().msg_id)
        .await
        .unwrap()
        .is_none());

    let renewed = renew(&w, app_id, &full, 2000);
    list(&w, app_id, &narrow, &renewed);
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    payer.receive(None).await.unwrap();
    assert_eq!(payer.app().cert().hash(), renewed.hash());
    // The renewal arrives during that read. Only the new certificate can read contacts.
    fail_list_read(&w);
    payer.receive(None).await.unwrap();
    assert!(payer
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_none());
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    payer.receive(None).await.unwrap();
    assert!(payer
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
    assert!(matches!(
        payer.app().delivery(&ended.message().msg_id).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    let received = shop.receive(None).await.unwrap();
    assert!(received.report.events.iter().any(|event| matches!(event,
        AppEvent::MessageReceived { msg_id, kind, .. } if *msg_id == ended.message().msg_id && kind == "paykit.allowance_end"
    )));
    assert!(payer
        .app()
        .protocol_scan("paykit", b"eo/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_allowances_crossing_end_rechecks_renewal_and_the_deny_copy_at_flush() {
    use pubky_switch_client::app::{Delivery, DeliveryOrigin};
    let w = World::new();
    let full = AppScopes::new()
        .allowance()
        .manage_contacts()
        .build()
        .unwrap();
    let mut payer = Box::pin(alice_app(&w, "wallet", full.clone())).await;
    let mut shop = Box::pin(shop(&w)).await;
    let (theirs, mine) = Box::pin(proposed(&w, &mut shop, &mut payer)).await;
    Box::pin(own_acceptance_recorded(&mut payer, &mine)).await;
    payer.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    payer.receive(None).await.unwrap();
    let ended = Box::pin(signed_recorded_end(&payer, &mine)).await;
    let msg_id = &ended.message().msg_id;
    let queued = payer.app().queued_messages("paykit").await.unwrap();
    let exact = queued.iter().find(|q| q.msg_id == *msg_id).unwrap();
    assert_eq!(exact.origin, DeliveryOrigin::Original);
    assert_eq!(exact.hash, ended.hash());
    assert!(exact.may_send);
    let original = payer.app().cert().clone();
    let app_id = payer.app_id();
    let narrow = renew(
        &w,
        app_id,
        &AppScopes::new().allowance().build().unwrap(),
        1000,
    );
    list(&w, app_id, &original, &narrow);
    payer.receive(None).await.unwrap();
    payer.app_mut().release_queued(&w.bob_id()).await.unwrap();
    assert!(matches!(
        payer.app_mut().flush_message(msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);

    payer.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let renewed = renew(&w, app_id, &full, 2000);
    list(&w, app_id, &narrow, &renewed);
    payer.receive(None).await.unwrap();
    payer.block(&w.bob_id()).await.unwrap();
    payer.app_mut().release_queued(&w.bob_id()).await.unwrap();
    assert!(matches!(
        payer.app_mut().flush_message(msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    payer.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    assert!(matches!(
        payer.app_mut().flush_message(msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
    payer.receive(None).await.unwrap();
    payer.app_mut().flush_message(msg_id).await.unwrap();
    assert!(matches!(
        payer.app().delivery(msg_id).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 1);
}

#[tokio::test]
async fn test_allowances_an_owed_end_without_its_origin_stays_held_after_restore() {
    let w = World::new();
    let mut payer = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let proposal = payer
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    payer.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let end = payer.end_allowance(&proposal.record).await.unwrap();
    let key = format!("end/{end}").into_bytes();
    assert!(payer
        .app()
        .protocol_get("paykit", &key)
        .await
        .unwrap()
        .is_some());
    // Restore a store that retained the signed stop and its outbox but lost its origin.
    payer
        .app_mut()
        .protocol_commit("paykit", &[LocalWrite::Delete { key }])
        .await
        .unwrap();
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut payer = Box::pin(restored_unreceived(&w, &backup)).await;
    Box::pin(read_restored_records(&mut payer)).await;
    payer.app_mut().release_queued(&w.bob_id()).await.unwrap();
    payer.app_mut().flush_outbox().await.unwrap();
    assert!(matches!(
        payer
            .allowance(&proposal.record)
            .await
            .unwrap_err()
            .refusal(),
        Some(Refusal::NotFound)
    ));
    let pending = pending_key(&payer, &w.bob_id(), &proposal.allowance_id);
    assert!(Box::pin(held_deleted(&payer, &pending)).await);
    assert!(payer
        .app()
        .shared_record(&proposal.record)
        .await
        .unwrap()
        .is_none());
    assert!(payer
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .iter()
        .any(|q| q.msg_id == end && !q.may_send));
    assert_eq!(Box::pin(taken(&mut shop, "paykit.allowance_end")).await, 0);
}

/// The selected original messages that reached this wallet on its next read.
async fn received_originals(wallet: &mut Paykit, ids: &[&str]) -> Vec<String> {
    wallet
        .receive(None)
        .await
        .unwrap()
        .report
        .events
        .into_iter()
        .filter_map(|event| match event {
            AppEvent::MessageReceived { msg_id, .. } if ids.contains(&msg_id.as_str()) => {
                Some(msg_id)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn test_allowances_queued_proposal_and_rejection_wait_for_the_current_deny_list() {
    let w = World::new();
    let peer = w.bob_id();
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let mut phone = Box::pin(keeper(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    wallet.app_mut().hold_queued(&peer).await.unwrap();
    let p = wallet
        .propose_executable_allowance(peer, terms(), UserConfirmed::new())
        .await
        .unwrap();
    let r = wallet.reject_allowance(&mine).await.unwrap();
    let ids = [p.msg_id.as_str(), r.as_str()];

    phone.block(&peer).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.app_mut().release_queued(&peer).await.unwrap();
    wallet.app_mut().flush_outbox().await.unwrap();
    assert!(Box::pin(received_originals(&mut shop, &ids))
        .await
        .is_empty());
    wallet.flush().await.unwrap();
    assert_eq!(
        wallet.allowance(&p.record).await.unwrap().blocked,
        Some(true)
    );
    assert!(Box::pin(received_originals(&mut shop, &ids))
        .await
        .is_empty());

    phone.unblock(&peer).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.flush().await.unwrap();
    assert_eq!(
        wallet.allowance(&p.record).await.unwrap().blocked,
        Some(false)
    );
    let mut got = Box::pin(received_originals(&mut shop, &ids)).await;
    got.sort();
    let mut expected = ids;
    expected.sort();
    assert_eq!(got, expected);
    wallet.flush().await.unwrap();
    assert!(Box::pin(received_originals(&mut shop, &ids))
        .await
        .is_empty());
}

#[tokio::test]
async fn test_allowances_user_end_and_resend_request_apply_their_own_delivery_rules() {
    let w = World::new();
    let peer = w.bob_id();
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let mut phone = Box::pin(keeper(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let p = wallet
        .propose_executable_allowance(peer, terms(), UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        Box::pin(received_originals(&mut shop, &[&p.msg_id])).await,
        [p.msg_id.as_str()]
    );
    wallet.app_mut().hold_queued(&peer).await.unwrap();
    let ask = wallet
        .request_resend(
            peer,
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap();
    let end = wallet.end_allowance(&p.record).await.unwrap();
    let ids = [end.as_str(), ask.as_str()];

    phone.block(&peer).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.flush().await.unwrap();
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&peer));
    wallet.app_mut().release_queued(&peer).await.unwrap();
    wallet.flush().await.unwrap();
    assert_eq!(
        Box::pin(received_originals(&mut shop, &ids)).await,
        [end.as_str()]
    );

    phone.unblock(&peer).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.flush().await.unwrap();
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&peer));
    assert_eq!(
        Box::pin(received_originals(&mut shop, &ids)).await,
        [ask.as_str()]
    );
    wallet.flush().await.unwrap();
    assert!(Box::pin(received_originals(&mut shop, &ids))
        .await
        .is_empty());
}

#[tokio::test]
async fn test_pending_reply_stop_holds_the_original_resend_across_a_cold_load() {
    Box::pin(held_pending_replies(PendingReplyCase::Retry)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_ends_with_the_verified_sibling_deletion() {
    Box::pin(held_pending_replies(PendingReplyCase::DeletionSeen)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_waits_if_only_the_recreated_proposal_is_seen() {
    Box::pin(held_pending_replies(PendingReplyCase::OnlyRecreationSeen)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_and_owed_acceptance_survive_an_older_in_place_restore() {
    Box::pin(held_pending_replies(PendingReplyCase::Restore)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_stays_after_the_unplaced_acceptance_is_given_up() {
    Box::pin(held_pending_replies(PendingReplyCase::GiveUp)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_requires_the_acceptance_to_come_first() {
    Box::pin(held_pending_replies(PendingReplyCase::RejectionFirst)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_retains_the_rejection_and_later_acceptance_until_deletion() {
    Box::pin(held_pending_replies(PendingReplyCase::RejectionRetry)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_never_deletes_a_recreated_proposal_for_an_earlier_rejection() {
    Box::pin(held_pending_replies(PendingReplyCase::RejectionReopened)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_keeps_a_recreation_first_read_after_the_rejection() {
    Box::pin(held_pending_replies(
        PendingReplyCase::RejectionReadsRecreation,
    ))
    .await;
}

#[tokio::test]
async fn test_pending_reply_stop_does_not_hide_the_give_up_of_a_lone_acceptance() {
    Box::pin(held_pending_replies(PendingReplyCase::LoneAcceptance)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_stays_retired_when_an_older_backup_is_restored_over_recreation() {
    Box::pin(held_pending_replies(PendingReplyCase::RestoreRetired)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_retries_a_native_stored_acceptance_after_an_older_restore() {
    Box::pin(held_pending_replies(PendingReplyCase::RestoreRetryable)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_excludes_a_lone_acceptance_after_deletion_and_recreation() {
    Box::pin(held_pending_replies(PendingReplyCase::RetireLone)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_excludes_a_parked_acceptance_before_deletion_and_recreation() {
    Box::pin(held_pending_replies(PendingReplyCase::RetireParked)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_orders_equal_millisecond_replies_across_a_known_loss() {
    Box::pin(pending_replies_across_loss(false, ReplyJourney::Placement)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_holds_equal_millisecond_replies_when_another_loss_obscures_order()
{
    Box::pin(pending_replies_across_loss(true, ReplyJourney::Placement)).await;
}

async fn accept_signed_proposal(
    world: &World,
    wallet: &Paykit,
    shop: &mut Paykit,
) -> (
    paykit_lib::AllowanceId,
    StoredMessage,
    pubky_switch_client::app::SignedMessage,
) {
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let offer = signed_pending_proposal(wallet, world.bob_id(), &id, "allower");
    world
        .bob
        .switch()
        .send_plaintext(
            world.alice_id(),
            wallet.app().cert(),
            offer.plaintext(),
            &[shop.app_id()],
        )
        .unwrap();
    let delivered = shop.app_mut().receive(None).await.unwrap();
    assert!(delivered.events.iter().any(|e| matches!(e, AppEvent::MessageReceived { msg_id, .. } if msg_id == &offer.message().msg_id)));
    let mut reply = OutgoingMessage::new(
        world.alice_id(),
        "paykit.allowance_acceptance",
        json!({"allowance_id":id.as_str()})
            .as_object()
            .unwrap()
            .clone(),
    );
    reply.refs = vec![offer.hash()];
    (id, offer, shop.app().sign_message(reply).unwrap())
}

#[tokio::test]
async fn test_pending_reply_stop_consumes_an_older_stored_answer_at_the_bound() {
    use pubky_switch_client::app::{Collection, ParkReason, RecordTarget};
    let w = World::new();
    let store = std::sync::Arc::new(crate::common::FailingRecordReads::default());
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        Default::default(),
        store.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = Box::pin(shop(&w)).await;
    Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    Box::pin(read_listed_records(&mut wallet)).await;
    assert!(wallet
        .app()
        .protocol_get("paykit", b"ct")
        .await
        .unwrap()
        .is_some());
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let deliver =
        |wallet: &Paykit, shop: &Paykit, reply: &pubky_switch_client::app::SignedMessage| {
            w.alice
                .switch()
                .send_plaintext(
                    w.bob_id(),
                    shop.app().cert(),
                    reply.plaintext(),
                    &[wallet.app_id()],
                )
                .unwrap();
        };
    let (id, proposal, first) = Box::pin(accept_signed_proposal(&w, &wallet, &mut shop)).await;
    let at = w.clock.now_ms();
    deliver(&wallet, &shop, &first);
    store.limit_next_cursor_time();
    let first_read = wallet.app_mut().receive(None).await.unwrap();
    assert!(
        first_read.events.iter().any(|event| matches!(
            event,
            AppEvent::Parked {
                reason: ParkReason::StoreRejected,
                ..
            }
        )),
        "{:?}",
        first_read.events
    );
    assert!(!Box::pin(keeps(&wallet, b"pr/", first.msg_id())).await);
    let mut others = Vec::new();
    for _ in 0..bound {
        w.clock.advance_ms(1);
        let (_, _, reply) = Box::pin(accept_signed_proposal(&w, &wallet, &mut shop)).await;
        deliver(&wallet, &shop, &reply);
        others.push(reply.msg_id().to_owned());
    }
    // A stored entry is retried before the feed. Keep the earlier input stored
    // while the sixty-four later answers are first admitted.
    store.limit_next_cursor_time();
    wallet.app_mut().receive(None).await.unwrap();
    for id in &others {
        assert!(Box::pin(keeps(&wallet, b"pr/", id)).await, "{id}");
    }
    assert!(!Box::pin(keeps(&wallet, b"pr/", first.msg_id())).await);
    wallet.app_mut().receive(None).await.unwrap();
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, bound);
    assert!(!Box::pin(keeps(&wallet, b"pr/", first.msg_id())).await);
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let marks = wallet.app().protocol_scan("paykit", b"pd/").await.unwrap();
    assert_eq!(marks.len(), 1);
    let mark: Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert_eq!(mark["d"]["earliest_ms"], at);
    assert_eq!(mark["d"]["latest_ms"], at);
    // The same signed reply is eligible again after the original proposal is held.
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    let pending = paykit_lib::records::PendingAllowanceRecord::new(&w.alice_id(), proposal)
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut wallet, &target, Some(&pending))).await;
    deliver(&wallet, &shop, &first);
    let read = wallet.app_mut().receive(None).await.unwrap();
    assert!(
        read.events.iter().any(
            |e| matches!(e, AppEvent::MessageReceived {msg_id,..} if msg_id == first.msg_id())
        ),
        "{:?}",
        read.events
    );
    assert!(Box::pin(keeps(&wallet, b"pr/", first.msg_id())).await);
    assert_eq!(Box::pin(kept_under(&wallet, b"pr/")).await, bound + 1);
}

#[tokio::test]
async fn test_pending_reply_stop_counts_replies_in_proven_feed_order_at_the_sender_bound() {
    Box::pin(pending_replies_across_loss(
        false,
        ReplyJourney::BeforeProposal,
    ))
    .await;
}

#[tokio::test]
async fn test_pending_reply_stop_keeps_indeterminate_replies_at_the_sender_bound() {
    Box::pin(pending_replies_across_loss(
        true,
        ReplyJourney::BeforeProposal,
    ))
    .await;
}

#[tokio::test]
async fn test_pending_reply_stop_protects_the_proven_first_answer_over_a_held_proposal_at_the_bound(
) {
    Box::pin(pending_replies_across_loss(
        false,
        ReplyJourney::OverProposal,
    ))
    .await;
}

enum ReplyJourney {
    Placement,
    BeforeProposal,
    OverProposal,
}

async fn pending_replies_across_loss(another_loss: bool, journey: ReplyJourney) {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::{Collection, FeedPosition, RecordTarget};
    use pubky_switch_client::testing::ScriptedSwitch;
    use std::sync::Arc;
    let w = World::new();
    let store: Arc<dyn pubky_switch_client::store::StateStore> =
        Arc::new(pubky_switch_client::store::MemoryStore::new());
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        Default::default(),
        store.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = Box::pin(shop(&w)).await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    // The phone records and delivers P while the sibling wallet is offline. After the first
    // service loss the two recipients still have P and this wallet has no copy to upload.
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let original = phone
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let p = Box::pin(pending_at(&phone, &original.record))
        .await
        .proposal()
        .clone();
    let id = original.allowance_id;
    let original_id = original.msg_id;
    drop(phone);
    for app in [&mut shop, &mut till] {
        let read = app.receive(None).await.unwrap();
        assert!(read
            .report
            .events
            .iter()
            .any(|e| matches!(e,AppEvent::MessageReceived{msg_id,..} if msg_id==&original_id)));
    }
    let pending = |id: &paykit_lib::AllowanceId| {
        let family = paykit_lib::Collection::AllowanceProposals;
        RecordTarget::new(
            Collection::new(family.name(), family.fixed_policy()),
            &paykit_lib::derive::natural_key::allowance(&w.bob_id(), id),
        )
    };
    let first_service = crate::provider_loss::declare_loss(&w);
    Box::pin(crate::provider_loss::follow_generation(&mut wallet)).await;
    let unseen_key = wallet.app().record_key(&pending(&id)).unwrap();
    assert!(wallet
        .app()
        .shared_record(&unseen_key)
        .await
        .unwrap()
        .is_none());
    let earlier_id = paykit_lib::AllowanceId::generate().unwrap();
    let earlier = signed_pending_proposal(&wallet, w.bob_id(), &earlier_id, "allower");
    let earlier = PendingAllowanceRecord::new(&w.alice_id(), earlier)
        .unwrap()
        .to_json()
        .unwrap();
    for _ in 0..20 {
        Box::pin(write_record(
            &mut wallet,
            &pending(&earlier_id),
            Some(&earlier),
        ))
        .await;
    }
    Box::pin(read_listed_records(&mut wallet)).await;
    let send = |app: &Paykit, service: &ScriptedSwitch, kind: &str, target: [u8; 16]| {
        let mut message = OutgoingMessage::new(
            w.alice_id(),
            kind,
            json!({"allowance_id":id.as_str()})
                .as_object()
                .unwrap()
                .clone(),
        );
        message.refs = vec![p.hash()];
        let signed = app.app().sign_message(message).unwrap();
        service
            .send_plaintext(w.bob_id(), app.app().cert(), signed.plaintext(), &[target])
            .unwrap();
        signed.msg_id().to_owned()
    };
    let q = send(
        &shop,
        &first_service,
        "paykit.allowance_acceptance",
        wallet.app_id(),
    );
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &q)).await);
    let replacement = crate::provider_loss::declare_second_loss(&w);
    Box::pin(crate::provider_loss::follow_generation(&mut wallet)).await;
    let r = send(
        &till,
        &replacement,
        "paykit.allowance_rejection",
        wallet.app_id(),
    );
    wallet.receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &r)).await);
    let held = wallet.app().protocol_scan("paykit", b"pr/").await.unwrap();
    let receipt = |id: &str| {
        let (_, bytes) = held
            .iter()
            .find(|(key, _)| key.ends_with(id.as_bytes()))
            .unwrap();
        let value: Value = serde_json::from_slice(bytes).unwrap();
        let value = &value["d"];
        (
            value["accepted_at"].as_u64().unwrap(),
            FeedPosition {
                generation: value["generation"].as_u64().unwrap(),
                seq: value["seq"].as_u64().unwrap(),
            },
        )
    };
    let (q_ms, q_pos) = receipt(&q);
    let (r_ms, r_pos) = receipt(&r);
    assert_eq!(q_ms, r_ms);
    assert!(q_pos.generation < r_pos.generation && q_pos.seq > r_pos.seq);
    let replacement = if another_loss {
        let next = crate::provider_loss::declare_third_loss(&w);
        Box::pin(crate::provider_loss::follow_generation(&mut wallet)).await;
        next
    } else {
        replacement
    };
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(restore_pending_wallet(&w, store, &backup)).await;
    let mut listed = false;
    for _ in 0..4 {
        let read = restored.receive(None).await.unwrap();
        listed = read.records_unlisted.is_none() && !restored.app().listing_owed().await.unwrap();
        if listed {
            break;
        }
    }
    assert!(listed);
    // The lost service's earlier, unrelated record still has no trusted current value.
    // The wallet can finish its listing and order the replies while the full view waits.
    let earlier_key = restored.app().record_key(&pending(&earlier_id)).unwrap();
    let missing = restored
        .app()
        .shared_record(&earlier_key)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !missing.authenticated
            && missing.flagged
            && missing.not_taken
            && missing.content.value().is_none()
    );
    assert!(matches!(
        restored.allowances().await.unwrap_err().refusal(),
        Some(Refusal::NotNow(_))
    ));

    assert_eq!(
        restored.app().received_order(q_pos, r_pos),
        (!another_loss).then_some(std::cmp::Ordering::Less)
    );
    if !matches!(journey, ReplyJourney::Placement) {
        let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
        let protected = matches!(journey, ReplyJourney::OverProposal);
        if protected {
            let current = PendingAllowanceRecord::new(&w.alice_id(), p.clone())
                .unwrap()
                .to_json()
                .unwrap();
            Box::pin(write_record(&mut restored, &pending(&id), Some(&current))).await;
        }
        let mut last = String::new();
        let mut first_offer = None;
        for _ in 0..(bound - 1 + usize::from(protected)) {
            w.clock.advance_ms(1);
            let (offer_id, offer, signed) =
                Box::pin(accept_signed_proposal(&w, &restored, &mut shop)).await;
            if first_offer.is_none() {
                first_offer = Some((offer_id, offer));
            }
            replacement
                .send_plaintext(
                    w.bob_id(),
                    shop.app().cert(),
                    signed.plaintext(),
                    &[restored.app_id()],
                )
                .unwrap();
            last = signed.msg_id().to_owned();
        }
        for _ in 0..4 {
            restored.app_mut().receive(None).await.unwrap();
        }
        assert_eq!(
            Box::pin(kept_under(&restored, b"pr/")).await,
            bound + usize::from(protected)
        );
        assert_eq!(
            Box::pin(keeps(&restored, b"pr/", &q)).await,
            another_loss || protected
        );
        assert_eq!(Box::pin(keeps(&restored, b"pr/", &r)).await, !protected);
        assert_eq!(
            Box::pin(keeps(&restored, b"pr/", &last)).await,
            !another_loss
        );
        if another_loss {
            let waiting = restored.app_mut().parked_entries().await.unwrap();
            assert_eq!(waiting.len(), 1);
            assert_eq!(
                waiting[0].reason,
                pubky_switch_client::app::ParkReason::AwaitingAdmission
            );
            let parked = restored
                .app_mut()
                .parked_entry(waiting[0].stored_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(parked.sender, w.bob_id());
            assert_eq!(parked.message.unwrap().msg_id, last);
            let (offer_id, offer) = first_offer.unwrap();
            let current = PendingAllowanceRecord::new(&w.alice_id(), offer)
                .unwrap()
                .to_json()
                .unwrap();
            Box::pin(write_record(
                &mut restored,
                &pending(&offer_id),
                Some(&current),
            ))
            .await;
            for _ in 0..3 {
                restored.app_mut().receive(None).await.unwrap();
            }
            assert!(restored
                .app_mut()
                .parked_entries()
                .await
                .unwrap()
                .is_empty());
            assert!(Box::pin(keeps(&restored, b"pr/", &last)).await);
            assert!(Box::pin(keeps(&restored, b"pr/", &q)).await);
            assert!(Box::pin(keeps(&restored, b"pr/", &r)).await);
            assert_eq!(Box::pin(kept_under(&restored, b"pr/")).await, bound + 1);
        }
        return;
    }
    let value = PendingAllowanceRecord::new(&w.alice_id(), p)
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut restored, &pending(&id), Some(&value))).await;
    for _ in 0..2 {
        restored.receive(None).await.unwrap();
    }
    let stops = restored
        .app()
        .protocol_scan("paykit", b"pc/")
        .await
        .unwrap();
    assert_eq!(stops.len(), usize::from(!another_loss));
    assert!(Box::pin(keeps(&restored, b"pr/", &q)).await);
    assert_eq!(Box::pin(keeps(&restored, b"pr/", &r)).await, another_loss);
    assert!(restored
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .is_empty());
    if another_loss {
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - 1);
        restored.receive(None).await.unwrap();
        assert!(Box::pin(keeps(&restored, b"pr/", &q)).await);
        w.clock.advance_ms(1);
        restored.receive(None).await.unwrap();
        assert!(!Box::pin(keeps(&restored, b"pr/", &q)).await);
        let notices = restored.notices().await.unwrap();
        let matches: Vec<_> = notices
            .iter()
            .filter(|n| {
                n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                    && n.msg_id.as_deref() == Some(q.as_str())
                    && n.counterparty == Some(w.bob_id())
            })
            .collect();
        assert_eq!(matches.len(), 1);
        restored.acknowledge_notice(&matches[0].id).await.unwrap();
        restored.receive(None).await.unwrap();
        assert!(restored
            .notices()
            .await
            .unwrap()
            .iter()
            .all(|n| n.msg_id.as_deref() != Some(q.as_str())));
        assert_eq!(Box::pin(kept_under(&restored, b"pc/")).await, 0);
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_reply_stop_binds_a_held_acceptance_to_a_verified_live_successor() {
    Box::pin(held_pending_replies(PendingReplyCase::LiveSuccessor)).await;
}

#[tokio::test]
async fn test_pending_reply_stop_holds_a_prior_acceptance_when_only_recreated_contents_match() {
    Box::pin(held_pending_replies(PendingReplyCase::UnwitnessedBoundary)).await;
}

#[tokio::test]
async fn test_pending_proposal_conflict_retries_after_opposed_replies_and_a_live_rewrite() {
    Box::pin(held_pending_replies(PendingReplyCase::CompetingProposal)).await;
}

enum PendingReplyCase {
    CompetingProposal,
    LiveSuccessor,
    UnwitnessedBoundary,
    RetireLone,
    RetireParked,
    Retry,
    DeletionSeen,
    OnlyRecreationSeen,
    RestoreRetired,
    RestoreRetryable,
    Restore,
    GiveUp,
    RejectionFirst,
    RejectionRetry,
    RejectionReopened,
    RejectionReadsRecreation,
    LoneAcceptance,
}

async fn restore_pending_wallet(
    world: &World,
    storage: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
    backup: &paykit_sdk::PaykitBackup,
) -> Paykit {
    Paykit::restore(
        world.env(&world.alice, storage),
        Default::default(),
        backup,
        world.rail.clone(),
        world.timer(),
    )
    .await
    .unwrap()
}

async fn held_pending_replies(by: PendingReplyCase) {
    use paykit_lib::records::PendingAllowanceRecord;
    use pubky_switch_client::app::Delivery;

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    let limited = std::sync::Arc::new(crate::common::FailingRecordReads::default());
    let storage: std::sync::Arc<dyn pubky_switch_client::store::StateStore> = limited.clone();
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        Default::default(),
        storage.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let id = paykit_lib::AllowanceId::generate().unwrap();
    let p = signed_pending_proposal(&wallet, w.bob_id(), &id, "allower");
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let permanent = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &id))
        .unwrap();
    let original = PendingAllowanceRecord::new(&w.alice_id(), p.clone())
        .unwrap()
        .to_json()
        .unwrap();
    Box::pin(write_record(&mut phone, &target, Some(&original))).await;
    Box::pin(read_listed_records(&mut wallet)).await;
    let selected = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(selected.content.value(), Some(original.as_slice()));
    // The shop receives the original envelope before either counterparty app responds.
    let original_id = wallet.app_mut().queue_stored(p.plaintext()).await.unwrap();
    assert_eq!(original_id, p.message().msg_id);
    wallet.flush().await.unwrap();
    for recipient in [&mut shop, &mut till] {
        let first = recipient.receive(None).await.unwrap();
        assert!(first.report.events.iter().any(|e| matches!(e,
            AppEvent::MessageReceived { kind, msg_id, .. }
                if kind == "paykit.allowance_proposal" && msg_id == &original_id)));
    }
    assert_ne!(shop.app_id(), till.app_id());
    let early = if matches!(
        by,
        PendingReplyCase::Restore | PendingReplyCase::RestoreRetryable
    ) {
        Some(wallet.backup().await.unwrap())
    } else {
        None
    };
    let rejection_first = matches!(
        by,
        PendingReplyCase::RejectionFirst
            | PendingReplyCase::RejectionRetry
            | PendingReplyCase::RejectionReopened
            | PendingReplyCase::RejectionReadsRecreation
    );
    let (first_kind, second_kind) = if rejection_first {
        ("paykit.allowance_rejection", "paykit.allowance_acceptance")
    } else {
        ("paykit.allowance_acceptance", "paykit.allowance_rejection")
    };
    inject_with_id(
        &w,
        &w.bob,
        "shop",
        &[wallet.app_id(), phone.app_id()],
        first_kind,
        vec![p.hash()],
        json!({"allowance_id":id.as_str()}),
        SENT_FIRST,
    );
    if matches!(
        by,
        PendingReplyCase::LiveSuccessor | PendingReplyCase::UnwitnessedBoundary
    ) {
        Box::pin(receive_without_allowance_view(&w, &mut wallet, &permanent)).await;
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        let old = wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .version;
        if matches!(by, PendingReplyCase::UnwitnessedBoundary) {
            Box::pin(write_record(&mut phone, &target, None)).await;
        }
        Box::pin(write_record(&mut phone, &target, Some(&original))).await;
        let read = wallet.app_mut().read_record_fresh(&target).await.unwrap();
        assert!(
            matches!(read,pubky_switch_client::app::RecordRead::Fresh(ref r) if r.authenticated && r.version>old && r.content.value()==Some(original.as_slice()))
        );
        inject_with_id(
            &w,
            &w.bob,
            "till",
            &[wallet.app_id()],
            second_kind,
            vec![p.hash()],
            json!({"allowance_id":id.as_str()}),
            SENT_SECOND,
        );
        for _ in 0..2 {
            wallet.receive(None).await.unwrap();
        }
        let stops = wallet.app().protocol_scan("paykit", b"pc/").await.unwrap();
        assert_eq!(
            stops.len(),
            usize::from(matches!(by, PendingReplyCase::LiveSuccessor))
        );
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    if matches!(
        by,
        PendingReplyCase::RetireLone | PendingReplyCase::RetireParked
    ) {
        if matches!(by, PendingReplyCase::RetireParked) {
            limited.limit_next_record(&key);
            wallet.receive(None).await.unwrap();
        } else {
            Box::pin(receive_without_allowance_view(&w, &mut wallet, &permanent)).await;
        }
        if matches!(by, PendingReplyCase::RetireParked) {
            let parked = wallet.app_mut().parked_entries().await.unwrap();
            assert_eq!(parked.len(), 1);
            let entry = wallet
                .app_mut()
                .parked_entry(parked[0].stored_id)
                .await
                .unwrap()
                .unwrap();
            assert!(entry.sender_verified);
            assert_eq!(entry.message.unwrap().msg_id, SENT_FIRST);
            assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        } else {
            assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        }
        assert!(wallet
            .app()
            .protocol_scan("paykit", b"pc/")
            .await
            .unwrap()
            .is_empty());
        Box::pin(write_record(&mut phone, &target, None)).await;
        let deleted = wallet.app_mut().read_record_fresh(&target).await.unwrap();
        assert!(
            matches!(deleted,pubky_switch_client::app::RecordRead::Fresh(ref record) if record.authenticated && record.content==pubky_switch_client::app::RecordContent::Deleted)
        );
        assert_eq!(
            wallet
                .app()
                .protocol_scan("paykit", b"pce/")
                .await
                .unwrap()
                .len(),
            1
        );
        Box::pin(write_record(&mut phone, &target, Some(&original))).await;
        let reopened = wallet.app_mut().read_record_fresh(&target).await.unwrap();
        let pubky_switch_client::app::RecordRead::Fresh(reopened) = reopened else {
            panic!("current recreated proposal")
        };
        assert!(reopened.authenticated && reopened.version > selected.version);
        assert_eq!(reopened.content.value(), Some(original.as_slice()));
        inject_with_id(
            &w,
            &w.bob,
            "till",
            &[wallet.app_id()],
            second_kind,
            vec![p.hash()],
            json!({"allowance_id":id.as_str()}),
            SENT_SECOND,
        );
        for _ in 0..3 {
            wallet.receive(None).await.unwrap();
        }
        let now = wallet.app().shared_record(&key).await.unwrap().unwrap();
        // This rejection arrived after the recreation. It is the first reply to that
        // lifetime; the earlier acceptance cannot pair with it to keep the proposal open.
        assert!(now.authenticated && now.version > reopened.version);
        assert_eq!(
            now.content,
            pubky_switch_client::app::RecordContent::Deleted
        );
        assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_SECOND)).await);
        assert!(wallet
            .app()
            .protocol_scan("paykit", b"pc/")
            .await
            .unwrap()
            .is_empty());
        assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
        assert!(wallet
            .app()
            .records_of("allowances")
            .await
            .unwrap()
            .is_empty());
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    let only_q = if matches!(by, PendingReplyCase::RestoreRetired) {
        Box::pin(receive_without_allowance_view(&w, &mut wallet, &permanent)).await;
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        Some(wallet.backup().await.unwrap())
    } else {
        None
    };
    let single = matches!(by, PendingReplyCase::LoneAcceptance);
    if !single {
        inject_with_id(
            &w,
            &w.bob,
            "till",
            &[wallet.app_id(), phone.app_id()],
            second_kind,
            vec![p.hash()],
            json!({"allowance_id":id.as_str()}),
            SENT_SECOND,
        );
    }
    if matches!(by, PendingReplyCase::RejectionReadsRecreation) {
        Box::pin(write_record(&mut phone, &target, None)).await;
        Box::pin(write_record(&mut phone, &target, Some(&original))).await;
        // The pending key GET observes the new write, with no intervening read of the delete.
        let new = wallet.app_mut().read_record_fresh(&target).await.unwrap();
        let pubky_switch_client::app::RecordRead::Fresh(live) = new else {
            panic!("current recreation")
        };
        assert!(live.version > selected.version);
        for _ in 0..2 {
            wallet.receive(None).await.unwrap();
        }
        let still_live = wallet.app().shared_record(&key).await.unwrap().unwrap();
        assert_eq!(still_live.version, live.version);
        assert_eq!(still_live.content.value(), Some(original.as_slice()));
        assert!(wallet
            .app()
            .records_of("allowances")
            .await
            .unwrap()
            .is_empty());
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    if matches!(
        by,
        PendingReplyCase::RejectionRetry | PendingReplyCase::RejectionReopened
    ) {
        refuse_writes(&w, 1);
        wallet.receive(None).await.unwrap();
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_SECOND)).await);
        let live = wallet.app().shared_record(&key).await.unwrap().unwrap();
        assert_eq!(live.version, selected.version);
        assert_eq!(live.content.value(), Some(original.as_slice()));
        let delivery = wallet.app_mut().resend(&original_id).await.unwrap();
        assert!(matches!(delivery, Delivery::Queued { .. }), "{delivery:?}");
        if matches!(by, PendingReplyCase::RejectionReopened) {
            Box::pin(write_record(&mut phone, &target, None)).await;
            Box::pin(write_record(&mut phone, &target, Some(&original))).await;
            let recreated = phone.app().shared_record(&key).await.unwrap().unwrap();
            assert!(recreated.version > selected.version);
            for _ in 0..2 {
                wallet.receive(None).await.unwrap();
            }
            let live = wallet.app().shared_record(&key).await.unwrap().unwrap();
            assert_eq!(live.version, recreated.version);
            assert_eq!(live.content.value(), Some(original.as_slice()));
            assert!(wallet
                .app()
                .records_of("allowances")
                .await
                .unwrap()
                .is_empty());
            assert!(w.rail.all_executions().is_empty());
            return;
        }
    }
    if matches!(by, PendingReplyCase::RestoreRetryable) {
        limited.limit_next_record(&key);
    }
    for app in [&mut wallet, &mut phone] {
        let summary = if single {
            Box::pin(receive_without_allowance_view(&w, app, &permanent)).await
        } else {
            app.receive(None).await.unwrap()
        };
        if matches!(by, PendingReplyCase::RestoreRetryable)
            && app.app_id() == w.alice.identity().app_keys("wallet").app_id_bytes()
        {
            let parked = app.app_mut().parked_entries().await.unwrap();
            assert_eq!(parked.len(), 1);
            assert_eq!(
                parked[0].reason,
                pubky_switch_client::app::ParkReason::StoreRejected
            );
            let q = app
                .app_mut()
                .parked_entry(parked[0].stored_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(q.message.unwrap().msg_id, SENT_FIRST);
            assert!(q.sender_verified);
            assert!(!Box::pin(keeps(app, b"se/", SENT_FIRST)).await);
            assert!(!Box::pin(keeps(app, b"pr/", SENT_FIRST)).await);
            assert!(Box::pin(keeps(app, b"pr/", SENT_SECOND)).await);
            continue;
        }
        if rejection_first {
            let closed = app.app().shared_record(&key).await.unwrap().unwrap();
            assert_eq!(
                closed.content,
                pubky_switch_client::app::RecordContent::Deleted
            );
            assert!(closed.version > selected.version);
            assert!(app
                .app()
                .protocol_scan("paykit", b"pc/")
                .await
                .unwrap()
                .is_empty());
            continue;
        }
        assert_eq!(
            app.app()
                .protocol_scan("paykit", b"pc/")
                .await
                .unwrap()
                .len(),
            usize::from(!single),
            "{:?}",
            summary.report.events
        );
        assert!(Box::pin(keeps(app, b"pr/", SENT_FIRST)).await);
        assert!(!Box::pin(keeps(app, b"pr/", SENT_SECOND)).await);
    }
    if matches!(by, PendingReplyCase::CompetingProposal) {
        Box::pin(write_record(&mut phone, &target, Some(&original))).await;
        let read = wallet.app_mut().read_record_fresh(&target).await.unwrap();
        assert!(
            matches!(read, pubky_switch_client::app::RecordRead::Fresh(ref held)
            if held.authenticated && held.version > selected.version && held.content.value() == Some(original.as_slice()))
        );
        // Bob closes his own received original before storing the competing proposal.
        // The user cancels delivery of this additional rejection while it is still held.
        let bob_record = shop
            .app()
            .record_key(&allowances_target(&w.alice_id(), &id))
            .unwrap();
        shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
        let unsent = shop.reject_allowance(&bob_record).await.unwrap();
        assert!(shop
            .app_mut()
            .withdraw_message("paykit", &unsent)
            .await
            .unwrap());
        shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
        let second = signed_pending_proposal(&shop, w.alice_id(), &id, "allowee");
        Box::pin(records_proposal(&mut shop, &second, &id)).await;
        let msg_id = shop
            .app_mut()
            .queue_stored(second.plaintext())
            .await
            .unwrap();
        assert_eq!(msg_id, second.message().msg_id);
        shop.flush().await.unwrap();
        for _ in 0..3 {
            wallet.receive(None).await.unwrap();
        }
        let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
        let current =
            PendingAllowanceRecord::read(held.content.value().unwrap(), &w.alice_id()).unwrap();
        assert_eq!(current.proposal().hash(), p.hash());
        assert_eq!(
            current.conflicted().map(StoredMessage::hash),
            Some(second.hash())
        );
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    if rejection_first {
        assert!(matches!(
            wallet.app_mut().resend(&original_id).await.unwrap(),
            Delivery::Queued { .. }
        ));
        let before = w.record_writes(&w.alice, &key);
        inject_with_id(
            &w,
            &w.bob,
            "shop",
            &[wallet.app_id()],
            "paykit.allowance_acceptance",
            vec![p.hash()],
            json!({"allowance_id":id.as_str()}),
            "7b3e9a1c-5d2f-4a8e-b6c4-1e9f3d7a2b58",
        );
        wallet.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.alice, &key), before);
        assert_eq!(
            wallet
                .app()
                .shared_record(&key)
                .await
                .unwrap()
                .unwrap()
                .content,
            pubky_switch_client::app::RecordContent::Deleted
        );
        assert!(wallet
            .app()
            .records_of("allowances")
            .await
            .unwrap()
            .is_empty());
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    if let Some(backup) = early {
        drop(wallet);
        let mut wallet = Box::pin(restore_pending_wallet(&w, storage, &backup)).await;
        if matches!(by, PendingReplyCase::RestoreRetryable) {
            Box::pin(read_restored_records(&mut wallet)).await;
            for _ in 0..2 {
                wallet.receive(None).await.unwrap();
            }
            let q = Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await;
            let stopped = Box::pin(kept_under(&wallet, b"pc/")).await;
            let r = Box::pin(keeps(&wallet, b"pr/", SENT_SECOND)).await;
            let native = wallet.app_mut().parked_entries().await.unwrap();
            w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + 1);
            wallet.receive(None).await.unwrap();
            let notices = wallet.notices().await.unwrap();
            let given_up = notices
                .iter()
                .filter(|n| {
                    n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                        && n.msg_id.as_deref() == Some(SENT_FIRST)
                })
                .count();
            assert_eq!(
                (q, stopped, r, native.len(), given_up),
                (true, 1, false, 0, 1),
                "post-restore custody of the retryable signed acceptance and its later rejection"
            );
            return;
        }
        assert_eq!(
            wallet
                .app()
                .protocol_scan("paykit", b"pc/")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await
                || Box::pin(keeps(&wallet, b"se/", SENT_FIRST)).await,
            "the unplaced acceptance must survive replacement of this installation"
        );
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    if single || matches!(by, PendingReplyCase::GiveUp) {
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + 1);
        wallet.receive(None).await.unwrap();
        assert!(!Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
        let notice = wallet
            .notices()
            .await
            .unwrap()
            .into_iter()
            .find(|n| {
                n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                    && n.msg_id.as_deref() == Some(SENT_FIRST)
                    && n.counterparty == Some(w.bob_id())
            })
            .unwrap();
        wallet.acknowledge_notice(&notice.id).await.unwrap();
        assert_eq!(
            wallet
                .app()
                .protocol_scan("paykit", b"pc/")
                .await
                .unwrap()
                .len(),
            usize::from(!single)
        );
        assert!(w.rail.all_executions().is_empty());
        return;
    }
    if matches!(by, PendingReplyCase::Retry) {
        let delivery = wallet.app_mut().resend(&original_id).await.unwrap();
        assert!(matches!(delivery, Delivery::Queued { .. }));
        let app_id = wallet.app_id();
        drop(wallet);
        let mut wallet =
            Box::pin(w.load_on_clock(&w.alice, storage, app_id, w.clock.clone())).await;
        wallet.flush().await.unwrap();
        let queued = wallet.app().queued_messages("paykit").await.unwrap();
        assert!(queued.iter().any(|q| q.msg_id == original_id
            && q.hash == p.hash()
            && q.to == w.bob_id()
            && q.resend));
        assert!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await);
    } else {
        let stale = if matches!(by, PendingReplyCase::RestoreRetired) {
            Some(wallet.backup().await.unwrap())
        } else {
            None
        };
        // The user's other wallet deletes its selected pending write on the service.
        Box::pin(write_record(&mut phone, &target, None)).await;
        let seen = matches!(
            by,
            PendingReplyCase::DeletionSeen | PendingReplyCase::RestoreRetired
        );
        if seen {
            wallet.app_mut().receive(None).await.unwrap();
            let r = wallet.app().shared_record(&key).await.unwrap().unwrap();
            assert!(r.authenticated && !r.not_taken && !r.doubtful);
            assert_eq!(r.content, pubky_switch_client::app::RecordContent::Deleted);
            assert!(wallet.app().record_seen(&key).await.unwrap().is_none());
        }
        let app_id = wallet.app_id();
        drop(wallet);
        let mut wallet =
            Box::pin(w.load_on_clock(&w.alice, storage.clone(), app_id, w.clock.clone())).await;
        phone.receive(None).await.unwrap();
        Box::pin(write_record(&mut phone, &target, Some(&original))).await;
        wallet.receive(None).await.unwrap();
        let r = wallet.app().shared_record(&key).await.unwrap().unwrap();
        assert_eq!(r.content.value(), Some(original.as_slice()));
        assert_eq!(Box::pin(keeps(&wallet, b"pr/", SENT_FIRST)).await, !seen);
        assert_eq!(
            wallet
                .app()
                .protocol_scan("paykit", b"pc/")
                .await
                .unwrap()
                .len(),
            usize::from(!seen)
        );
        if let Some(backup) = stale {
            drop(wallet);
            let mut restored = Box::pin(restore_pending_wallet(&w, storage.clone(), &backup)).await;
            Box::pin(read_restored_records(&mut restored)).await;
            let current = restored.app().shared_record(&key).await.unwrap().unwrap();
            assert_eq!(current.content.value(), Some(original.as_slice()));
            assert!(
                restored
                    .app()
                    .protocol_scan("paykit", b"pc/")
                    .await
                    .unwrap()
                    .is_empty(),
                "the restored layer reinstated the earlier rejection over the current proposal"
            );
            assert!(
                !Box::pin(keeps(&restored, b"pr/", SENT_FIRST)).await,
                "the restored layer reinstated a retired placement duty"
            );
            let fresh_store = crate::common::store();
            let fresh = Box::pin(restore_pending_wallet(&w, fresh_store.clone(), &backup)).await;
            assert_eq!(Box::pin(kept_under(&fresh, b"pc/")).await, 1);
            assert!(Box::pin(keeps(&fresh, b"pr/", SENT_FIRST)).await);
            // Restore a layer from before R. Its only duty still names the earlier P.
            drop(restored);
            let only_q = only_q.unwrap();
            let mut restored = Box::pin(restore_pending_wallet(&w, storage.clone(), &only_q)).await;
            assert!(!Box::pin(keeps(&restored, b"pr/", SENT_FIRST)).await);
            Box::pin(read_restored_records(&mut restored)).await;
            let closed = restored.backup().await.unwrap();
            drop(fresh);
            let handed_off =
                Box::pin(restore_pending_wallet(&w, fresh_store.clone(), &closed)).await;
            assert_eq!(Box::pin(kept_under(&handed_off, b"pce/")).await, 1);
            drop(handed_off);
            for selected in [&backup, &only_q] {
                let again =
                    Box::pin(restore_pending_wallet(&w, fresh_store.clone(), selected)).await;
                assert_eq!(Box::pin(kept_under(&again, b"pc/")).await, 0);
                assert_eq!(Box::pin(kept_under(&again, b"pce/")).await, 1);
                assert!(!Box::pin(keeps(&again, b"pr/", SENT_FIRST)).await);
            }
            let selected = restored
                .app_mut()
                .read_record(&target)
                .await
                .unwrap()
                .unwrap();
            assert!(
                selected.authenticated
                    && !selected.not_taken
                    && !selected.doubtful
                    && !selected.divergent
                    && !selected.flagged
            );
            assert_eq!(selected.version, r.version);
            assert_eq!(selected.content.value(), Some(original.as_slice()));
            let preceding: Vec<_> = restored
                .app()
                .protocol_scan("paykit", b"pr/")
                .await
                .unwrap()
                .into_iter()
                .map(|(k, _)| String::from_utf8(k).unwrap())
                .collect();
            let second_q = "d6723437-48b0-45ca-9c7e-cdce3a9b788f";
            for (sender, kind, msg_id) in [
                ("shop", first_kind, second_q),
                ("till", second_kind, SORTS_FIRST),
            ] {
                inject_with_id(
                    &w,
                    &w.bob,
                    sender,
                    &[restored.app_id(), phone.app_id()],
                    kind,
                    vec![p.hash()],
                    json!({"allowance_id":id.as_str()}),
                    msg_id,
                );
            }
            let got = restored.receive(None).await.unwrap();
            assert!(Box::pin(keeps(&restored, b"pr/", second_q)).await);
            assert_eq!(
                Box::pin(kept_under(&restored, b"pc/")).await,
                1,
                "verified {:?}, prior replies {:?}, events {:?}",
                selected.version,
                preceding,
                got.report.events
            );
            let later = restored.backup().await.unwrap();
            drop(restored);
            let mut restored = Box::pin(restore_pending_wallet(&w, storage.clone(), &backup)).await;
            assert_eq!(Box::pin(kept_under(&restored, b"pc/")).await, 1);
            assert!(Box::pin(keeps(&restored, b"pr/", second_q)).await);
            assert!(!Box::pin(keeps(&restored, b"pr/", SENT_FIRST)).await);
            Box::pin(read_restored_records(&mut restored)).await;
            phone.receive(None).await.unwrap();
            Box::pin(write_record(&mut phone, &target, None)).await;
            restored.receive(None).await.unwrap();
            assert_eq!(Box::pin(kept_under(&restored, b"pc/")).await, 0);
            drop(restored);
            for selected in [&later, &backup, &only_q] {
                let again = Box::pin(restore_pending_wallet(&w, storage.clone(), selected)).await;
                assert_eq!(Box::pin(kept_under(&again, b"pce/")).await, 1);
                assert_eq!(Box::pin(kept_under(&again, b"pc/")).await, 0);
                assert_eq!(Box::pin(kept_under(&again, b"pr/")).await, 0);
            }
        }
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowances_dropped_reply_waits_for_contacts_and_a_current_deny_copy() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(alice_app(
        &w,
        "wallet",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    let mut controller = w
        .join(
            &w.alice,
            "controller",
            AppScopes::new()
                .allowance()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let recipient = w.bob_id();
    let at = w.clock.now_ms();
    for _ in 0..=paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        let id = paykit_lib::AllowanceId::generate().unwrap();
        let proposal = json!({
            "allowance_id": id.as_str(), "proposer_role": "allower", "allower_generation": 1,
            "terms": terms().to_json().unwrap(),
        });
        let signed = controller
            .app()
            .sign_message(OutgoingMessage::new(
                recipient,
                "paykit.allowance_proposal",
                proposal.as_object().cloned().unwrap(),
            ))
            .unwrap();
        w.inject(
            &w.bob,
            "shop",
            &w.alice,
            &[wallet.app_id()],
            "paykit.allowance_rejection",
            vec![signed.hash()],
            json!({"allowance_id": id.as_str()})
                .as_object()
                .cloned()
                .unwrap(),
        );
    }
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER
    );
    assert_eq!(
        crate::common::mark_senders(&wallet, "pd/").await,
        [recipient]
    );
    assert_eq!(summary.resends, 0);
    assert!(
        matches!(summary.resend_refused, Some(Refusal::ScopeMissing(ref scope)) if scope == "paykit_contacts")
    );
    let err = wallet
        .request_resend(
            recipient,
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(at),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.refusal(), Some(Refusal::ScopeMissing(scope)) if scope == "paykit_contacts"),
        "{err}"
    );
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .iter()
        .all(|message| message.kind != "paykit.resend_request"));
    assert!(crate::common::asked(&wallet, &recipient).await.is_empty());
    controller.block(&recipient).await.unwrap();
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().allowance().contacts().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    store_bundle(&w, me, &wider);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    wallet.app_mut().flush_outbox().await.unwrap();
    assert!(crate::common::asked(&wallet, &recipient).await.is_empty());
    controller.unblock(&recipient).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        crate::common::asked(&wallet, &recipient).await,
        vec![(Some(at), None)]
    );
    assert_eq!(Box::pin(taken(&mut shop, "paykit.resend_request")).await, 1);
}

#[tokio::test]
async fn test_pending_proposals_in_both_roles_use_the_specified_addresses() {
    let w = World::new();
    let both = || {
        AppScopes::new()
            .request()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap()
    };
    let mut wallet = Box::pin(alice_app(&w, "wallet", both())).await;
    let mut shop = w.join(&w.bob, "shop", both()).await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    for app in [&mut wallet, &mut shop] {
        Box::pin(read_listed_records(app)).await;
    }
    for role in [AllowanceRole::Allower, AllowanceRole::Allowee] {
        let sent = propose_in_role(&mut wallet, w.bob_id(), role)
            .await
            .unwrap();
        shop.receive(None).await.unwrap();
        let seen = shop
            .allowances()
            .await
            .unwrap()
            .into_iter()
            .find(|view| view.allowance_id == sent.allowance_id)
            .unwrap();
        for (app, peer, public) in [
            (&wallet, w.bob_id(), sent.record),
            (&shop, w.alice_id(), seen.record),
        ] {
            Box::pin(assert_pending_address(
                app,
                peer,
                &sent.allowance_id,
                public,
                false,
            ))
            .await;
        }
        shop.reject_allowance(&seen.record).await.unwrap();
        wallet.receive(None).await.unwrap();
        for (app, peer, public) in [
            (&wallet, w.bob_id(), sent.record),
            (&shop, w.alice_id(), seen.record),
        ] {
            Box::pin(assert_pending_address(
                app,
                peer,
                &sent.allowance_id,
                public,
                true,
            ))
            .await;
        }
    }
    assert!(w.rail.all_executions().is_empty());
}

async fn assert_pending_address(
    app: &Paykit,
    peer: [u8; 32],
    id: &paykit_lib::AllowanceId,
    public: [u8; 32],
    deleted: bool,
) {
    use pubky_switch_client::app::{Collection, RecordContent, RecordTarget};
    use pubky_switch_core::{
        ident::Policy,
        ops::{Condition, OpKind, RecordVersion, SignedOp},
    };
    let own = app.identity();
    let natural = paykit_lib::derive::natural_key::allowance(&peer, id);
    let ordinary = Policy::new("paykit_allowance", &["paykit_allowance"]);
    let pending = RecordTarget::new(
        Collection::new("allowance_proposals", ordinary.clone()),
        &natural,
    );
    let permanent = RecordTarget::new(
        Collection::new("allowances", ordinary.permanent(None)),
        &natural,
    );
    let permanent_key = app.app().record_key(&permanent).unwrap();
    let pending_key = app.app().record_key(&pending).unwrap();
    assert_eq!(permanent_key, public);
    assert_ne!(pending_key, public);
    let record = app
        .app()
        .shared_record(&pending_key)
        .await
        .unwrap()
        .unwrap();
    assert!(
        record.authenticated
            && !record.not_taken
            && !record.flagged
            && !record.doubtful
            && !record.divergent
    );
    assert_eq!(record.content == RecordContent::Deleted, deleted);
    assert!(app
        .app()
        .shared_record(&permanent_key)
        .await
        .unwrap()
        .is_none());
    let ops = app.app().shared_evidence().await.unwrap().ops;
    let op = ops
        .iter()
        .find_map(|held| {
            let cert = paykit_lib::scopes::verify_certificate(&held.cert, &own).ok()?;
            let op = SignedOp::verify_with_cert(&held.op, &cert, &own).ok()?;
            let OpKind::RecordPut(put) = &op.body().kind else {
                return None;
            };
            (put.key == pending_key && (put.expected_version != RecordVersion::NONE) == deleted)
                .then_some(op)
        })
        .expect("the wallet retains the selected signed pending write");
    let absent = Condition::RecordVersion {
        key: permanent_key,
        version: RecordVersion::NONE,
    };
    assert!(
        op.body().conditions.contains(&absent),
        "the pending write must guard the permanent Allowance key"
    );
}

#[tokio::test]
async fn test_pending_proposal_received_again_after_its_signed_acceptance_stays_closed() {
    let w = World::new();
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let sent = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let permanent = shop
        .app()
        .record_key(&allowances_target(&w.alice_id(), &sent.allowance_id))
        .unwrap();
    let original = Box::pin(pending_at(&shop, &permanent))
        .await
        .proposal()
        .clone();
    let acceptance = Box::pin(own_acceptance_recorded(&mut shop, &permanent)).await;
    let pending = pending_key(&shop, &w.alice_id(), &sent.allowance_id);
    let accepted_value = Box::pin(raw(&shop, &permanent)).await;
    let before = (
        w.record_writes(&w.bob, &pending),
        w.record_writes(&w.bob, &permanent),
    );

    let scopes = AppScopes::new().request().allowance().build().unwrap();
    let mut counter = w.join(&w.bob, "counter", scopes).await;
    counter.app_mut().invite(&w.alice_id()).await.unwrap();
    w.bob
        .switch()
        .send_plaintext(
            w.alice_id(),
            original.cert(),
            original.plaintext(),
            &[counter.app_id()],
        )
        .unwrap();
    for _ in 0..3 {
        counter.receive(None).await.unwrap();
    }
    let view = counter.allowance(&permanent).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Accepted);
    assert!(!view.consented && view.executor_app_id.is_none());
    assert!(counter.reject_allowance(&permanent).await.is_err());
    assert!(counter.discard_pending_allowance(&permanent).await.is_err());
    let after = Box::pin(raw(&counter, &permanent)).await;
    assert_eq!(after, accepted_value);
    let checked = AllowanceRecord::read(&after, &w.bob_id()).unwrap();
    assert_eq!(checked.proposal.hash(), original.hash());
    assert_eq!(checked.response.unwrap().hash(), acceptance.hash());
    assert_eq!(
        before,
        (
            w.record_writes(&w.bob, &pending),
            w.record_writes(&w.bob, &permanent)
        )
    );
    assert!(counter
        .app()
        .shared_record(&pending)
        .await
        .unwrap()
        .is_none_or(|held| held.content == pubky_switch_client::app::RecordContent::Deleted));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_withdrawal_before_the_recipient_has_placed_the_proposal() {
    Box::pin(withdraw_before_receiving(WithdrawalCase::Together)).await;
}

#[tokio::test]
async fn test_pending_withdrawal_cannot_close_a_later_identical_recreation() {
    Box::pin(withdraw_before_receiving(WithdrawalCase::Recreated)).await;
}

#[tokio::test]
async fn test_queued_withdrawal_after_reload_keeps_identical_recreated_proposals() {
    Box::pin(queued_withdrawal_after_reload(LaterProposal::Pending)).await;
}

/// Stop after the confirmed deletion and before Paykit releases the original signed message.
/// An orderly change keeps the service history; an identity without the old key cannot date it.
async fn applied_answer_before_key_change(withdraw: bool, retain_key: bool) {
    use crate::common::ALICE_ORIGIN;
    use pubky_switch_client::app::{Delivery, RecordContent};
    use pubky_switch_client::store::{Fault, FaultStore};
    use pubky_switch_core::records::{Inbox, ServiceKeyEntry};
    let w = World::new();
    let fault = std::sync::Arc::new(FaultStore::new(crate::common::store(), Fault::None));
    let scopes = AppScopes::new().pay().allowance().manage_contacts();
    let scopes = if withdraw {
        scopes.allowance_execute()
    } else {
        scopes
    };
    let scopes = scopes.build().unwrap();
    let mut payer = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        scopes,
        Default::default(),
        fault.clone(),
    ))
    .await;
    payer.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut seller = Box::pin(shop(&w)).await;
    let (public, id) = if withdraw {
        let sent = payer
            .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
            .await
            .unwrap();
        seller.receive(None).await.unwrap();
        (sent.record, sent.allowance_id)
    } else {
        let sent = seller
            .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
            .await
            .unwrap();
        payer.receive(None).await.unwrap();
        (
            payer
                .app()
                .record_key(&allowances_target(&w.bob_id(), &sent.allowance_id))
                .unwrap(),
            sent.allowance_id,
        )
    };
    let key = pending_key(&payer, &w.bob_id(), &id);
    let before = payer.app().shared_record(&key).await.unwrap().unwrap();
    let app_id = payer.app_id();
    let old = payer.app().own_identity().inbox().service_key;
    let generation = payer.app().own_identity().generation();
    fault.fail_put_once(b"p/paykit/aq/");
    let answer = if withdraw {
        payer.end_allowance(&public).await
    } else {
        payer.reject_allowance(&public).await
    };
    assert!(answer.is_err());
    let (message, hash) = Box::pin(prepared_answer(&payer)).await;
    assert!(matches!(
        payer.app().delivery(&message).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    let marker = format!("aq/{}", pubky_switch_core::enc::b64(&hash));
    assert!(payer
        .app()
        .protocol_get("paykit", marker.as_bytes())
        .await
        .unwrap()
        .is_none());
    let closed = payer.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        closed.authenticated
            && closed.version.generation == generation
            && closed.version.version > before.version.version
    );
    assert_eq!(closed.content, RecordContent::Deleted);
    assert!(Box::pin(received_originals(&mut seller, &[&message]))
        .await
        .is_empty());
    drop(payer);
    let service = w.alice.switch().change_key_into([74; 32]);
    w.router.add_switch(ALICE_ORIGIN, &service);
    let doc = w
        .alice
        .sign_next(|body| {
            if retain_key {
                body.service_keys.push(ServiceKeyEntry {
                    generation,
                    key: old,
                });
            }
            body.generation += 1;
            body.inbox = Inbox::new(ALICE_ORIGIN, service.service_key());
        })
        .unwrap();
    service.deliver_identity(&doc).unwrap();
    let mut payer = Box::pin(Paykit::load(
        w.env(&w.alice, fault.inner()),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    payer.app_mut().refresh_identity().await.unwrap();
    assert_eq!(payer.app().own_identity().generation(), generation + 1);
    assert_eq!(
        payer.app().own_identity().service_key_of(generation),
        retain_key.then_some(old)
    );
    assert!(payer.app().own_identity().body().recovery_from.is_none());
    Box::pin(payer.flush()).await.unwrap();
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    Box::pin(payer.flush()).await.unwrap();
    let received = Box::pin(received_originals(&mut seller, &[&message])).await;
    if retain_key {
        assert_eq!(received, std::slice::from_ref(&message));
        assert!(matches!(
            payer.app().delivery(&message).await.unwrap(),
            Some(Delivery::Accepted { .. })
        ));
        let bytes = payer
            .app()
            .protocol_get("paykit", marker.as_bytes())
            .await
            .unwrap()
            .unwrap();
        let mark: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(mark["d"]["retirement"]["generation"], generation);
        assert_eq!(mark["d"]["retirement"]["version"], closed.version.version);
        Box::pin(payer.flush()).await.unwrap();
        assert!(Box::pin(received_originals(&mut seller, &[&message]))
            .await
            .is_empty());
    } else {
        assert!(received.is_empty());
        assert!(payer
            .app()
            .protocol_get("paykit", marker.as_bytes())
            .await
            .unwrap()
            .is_none());
        assert_eq!(Box::pin(prepared_answer(&payer)).await, (message, hash));
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_applied_rejection_before_an_orderly_key_change_is_delivered_on_reopen() {
    Box::pin(applied_answer_before_key_change(false, true)).await;
}
#[tokio::test]
async fn test_applied_withdrawal_before_an_orderly_key_change_is_delivered_on_reopen() {
    Box::pin(applied_answer_before_key_change(true, true)).await;
}
#[tokio::test]
async fn test_applied_rejection_without_the_previous_service_key_stays_held() {
    Box::pin(applied_answer_before_key_change(false, false)).await;
}
#[tokio::test]
async fn test_applied_withdrawal_without_the_previous_service_key_stays_held() {
    Box::pin(applied_answer_before_key_change(true, false)).await;
}

#[tokio::test]
async fn test_kept_rejection_delivers_after_a_service_key_change_preserves_its_selected_proposal() {
    Box::pin(kept_rejection_across_generations(false)).await;
}

#[tokio::test]
async fn test_kept_rejection_refuses_the_pre_loss_condition_on_an_uploaded_proposal() {
    Box::pin(kept_rejection_across_generations(true)).await;
}

async fn kept_rejection_across_generations(lost: bool) {
    use pubky_switch_client::app::{Delivery, KeptBatchStatus, RecordContent, RecordRead};
    use pubky_switch_client::store::MemoryStore;
    use std::sync::Arc;

    let w = World::new();
    let store = Arc::new(MemoryStore::new());
    let scopes = AppScopes::new()
        .pay()
        .allowance()
        .manage_contacts()
        .build()
        .unwrap();
    let mut payer = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        scopes.clone(),
        Default::default(),
        store.clone(),
    ))
    .await;
    let mut sibling = Box::pin(alice_app(&w, "phone", scopes)).await;
    let mut seller = Box::pin(shop(&w)).await;
    payer.app_mut().invite(&w.bob_id()).await.unwrap();
    let offer = seller
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    payer.receive(None).await.unwrap();
    let target = pending_target(&w.bob_id(), &offer.allowance_id);
    let canonical = payer
        .app()
        .record_key(&allowances_target(&w.bob_id(), &offer.allowance_id))
        .unwrap();
    let RecordRead::Fresh(proposal) = sibling.app_mut().read_record_fresh(&target).await.unwrap()
    else {
        panic!("the sibling must hold the authenticated proposal before recovery");
    };
    assert!(proposal.authenticated && proposal.content.value().is_some());

    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "ops".into(),
            after: 0,
        });
    let id = payer.reject_allowance(&canonical).await.unwrap();
    let Some(Delivery::Kept { batch_id }) = payer.app().delivery(&id).await.unwrap() else {
        panic!("the original rejection must still be in its unanswered batch");
    };
    let app_id = payer.app_id();
    drop(payer);
    if lost {
        crate::provider_loss::declare_loss(&w);
        Box::pin(crate::provider_loss::follow_generation(&mut sibling)).await;
        // An upload preserves P's original version but acquires a new service revision.
        let RecordRead::Fresh(uploaded) =
            sibling.app_mut().read_record_fresh(&target).await.unwrap()
        else {
            panic!("the replacement must have the proposal the payer actually selected");
        };
        assert!(uploaded.authenticated);
        assert_eq!(
            (uploaded.version, &uploaded.content),
            (proposal.version, &proposal.content)
        );
    } else {
        use crate::common::ALICE_ORIGIN;
        use pubky_switch_core::records::Inbox;
        // Switch 12.5 retains the current signed write across a service key change.
        let service = w.alice.switch().change_key_into([74; 32]);
        w.router.add_switch(ALICE_ORIGIN, &service);
        let doc = w
            .alice
            .sign_next(|body| {
                body.generation += 1;
                body.inbox = Inbox::new(ALICE_ORIGIN, service.service_key());
            })
            .unwrap();
        service.deliver_identity(&doc).unwrap();
    }

    let mut payer = Box::pin(Paykit::load(
        w.env(&w.alice, store),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    payer.app_mut().refresh_identity().await.unwrap();
    for _ in 0..6 {
        payer.receive(None).await.unwrap();
    }
    let current_generation = payer.app().own_identity().generation();
    assert!(current_generation > proposal.version.generation);
    let outcome = payer.app().kept_batch(batch_id).await.unwrap();
    if lost {
        assert!(
            matches!(outcome, Some(KeptBatchStatus::Dropped {
            reason: pubky_switch_client::app::KeptBatchDrop::Refused { ref code }, ..
        }) if code == "conflict"),
            "{outcome:?}"
        );
        payer.flush().await.unwrap();
        assert!(Box::pin(received_originals(&mut seller, &[&id]))
            .await
            .is_empty());
        let RecordRead::Fresh(live) = payer.app_mut().read_record_fresh(&target).await.unwrap()
        else {
            panic!("the refused deletion must leave the re-uploaded P intact");
        };
        assert!(live.authenticated);
        assert_eq!(
            (live.version, live.content),
            (proposal.version, proposal.content)
        );
        return;
    }
    assert!(
        matches!(outcome, Some(KeptBatchStatus::Applied { .. })),
        "the original guarded deletion must have applied after recovery: {outcome:?}"
    );
    let closed = payer
        .app()
        .shared_record(&proposal.key)
        .await
        .unwrap()
        .unwrap();
    assert!(closed.authenticated && closed.version.generation == current_generation);
    assert_eq!(closed.content, RecordContent::Deleted);
    payer.flush().await.unwrap();
    assert!(matches!(
        payer.app().delivery(&id).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    assert_eq!(
        Box::pin(received_originals(&mut seller, &[&id]))
            .await
            .as_slice(),
        std::slice::from_ref(&id)
    );
    assert!(Box::pin(received_originals(&mut seller, &[&id]))
        .await
        .is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_queued_withdrawal_after_provider_loss_preserves_the_peers_proposal() {
    use pubky_switch_client::app::Delivery;

    let w = World::new();
    let mut payer = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let first = payer
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let local = pending_target(&w.bob_id(), &first.allowance_id);
    let peer = pending_target(&w.alice_id(), &first.allowance_id);
    let peer_key = shop.app().record_key(&peer).unwrap();
    let held = shop.app().shared_record(&peer_key).await.unwrap().unwrap();
    assert!(held.authenticated && held.content.value().is_some());
    payer.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let old = payer.end_allowance(&first.record).await.unwrap();
    let local_key = payer.app().record_key(&local).unwrap();
    let deleted = payer
        .app()
        .shared_record(&local_key)
        .await
        .unwrap()
        .unwrap();
    assert!(deleted.authenticated);
    assert_eq!(
        deleted.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    // The peer already holds a later incarnation before the sender changes provider.
    let remote = shop
        .app()
        .record_key(&allowances_target(&w.alice_id(), &first.allowance_id))
        .unwrap();
    let original = Box::pin(pending_at(&shop, &remote)).await;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    shop.reject_allowance(&remote).await.unwrap();
    Box::pin(records_proposal(
        &mut shop,
        original.proposal(),
        &first.allowance_id,
    ))
    .await;
    let newer = shop.app().shared_record(&peer_key).await.unwrap().unwrap();
    assert!(newer.authenticated && newer.version > held.version);
    assert_eq!(newer.content, held.content);
    crate::provider_loss::declare_loss(&w);
    Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    assert_eq!(
        payer.app().own_identity().body().recovery_from,
        Some(deleted.version.generation)
    );
    payer.app_mut().release_queued(&w.bob_id()).await.unwrap();
    payer.flush().await.unwrap();
    assert!(matches!(
        payer.app_mut().flush_message(&old).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    let received = shop.receive(None).await.unwrap();
    assert!(!received
        .report
        .events
        .iter()
        .any(|event| matches!(event, AppEvent::MessageReceived {msg_id, ..} if msg_id == &old)));
    let current = shop.app().shared_record(&peer_key).await.unwrap().unwrap();
    assert_eq!(
        (current.version, current.content),
        (newer.version, newer.content.clone())
    );
    w.clock.advance_ms(60_000);
    assert!(matches!(
        payer.app_mut().flush_message(&old).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert!(Box::pin(received_originals(&mut shop, &[&old]))
        .await
        .is_empty());
    let current = shop.app().shared_record(&peer_key).await.unwrap().unwrap();
    assert_eq!(
        (current.version, current.content),
        (newer.version, newer.content)
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_queued_withdrawal_after_reload_cannot_use_the_next_deletion() {
    Box::pin(queued_withdrawal_after_reload(LaterProposal::Withdrawn)).await;
}

#[tokio::test]
async fn test_queued_withdrawal_after_reload_keeps_the_permanent_acceptance() {
    Box::pin(queued_withdrawal_after_reload(LaterProposal::Accepted)).await;
}

enum LaterProposal {
    Pending,
    Withdrawn,
    Accepted,
}

async fn queued_withdrawal_after_reload(later: LaterProposal) {
    use pubky_switch_client::app::Delivery;
    use pubky_switch_client::store::MemoryStore;
    use std::sync::Arc;

    let w = World::new();
    let store = Arc::new(MemoryStore::new());
    let scopes = AppScopes::new()
        .pay()
        .allowance()
        .allowance_execute()
        .manage_contacts()
        .build()
        .unwrap();
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        scopes.clone(),
        Default::default(),
        store.clone(),
    ))
    .await;
    let mut sibling = Box::pin(alice_app(&w, "phone", scopes)).await;
    let mut shop = Box::pin(shop(&w)).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let proposal = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let local = pending_target(&w.bob_id(), &proposal.allowance_id);
    let remote = pending_target(&w.alice_id(), &proposal.allowance_id);
    let local_key = wallet.app().record_key(&local).unwrap();
    let remote_key = shop.app().record_key(&remote).unwrap();
    let original = Box::pin(pending_at(&wallet, &proposal.record)).await;
    let remote_public = shop
        .app()
        .record_key(&allowances_target(&w.alice_id(), &proposal.allowance_id))
        .unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let withdrawal = wallet.end_allowance(&proposal.record).await.unwrap();
    assert!(matches!(
        wallet.app().delivery(&withdrawal).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    let old_delete = wallet
        .app()
        .shared_record(&local_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        old_delete.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    let app_id = wallet.app_id();
    drop(wallet);

    // Both accounts again place the very same signed P under the same Allowance ID.
    sibling.app_mut().read_record_fresh(&local).await.unwrap();
    Box::pin(records_proposal(
        &mut sibling,
        original.proposal(),
        &proposal.allowance_id,
    ))
    .await;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    shop.reject_allowance(&remote_public).await.unwrap();
    Box::pin(records_proposal(
        &mut shop,
        original.proposal(),
        &proposal.allowance_id,
    ))
    .await;
    match later {
        LaterProposal::Pending => (),
        LaterProposal::Withdrawn => {
            sibling.app_mut().hold_queued(&w.bob_id()).await.unwrap();
            sibling.end_allowance(&proposal.record).await.unwrap();
        }
        LaterProposal::Accepted => {
            let q = Box::pin(own_acceptance_recorded(&mut shop, &remote_public)).await;
            Box::pin(install_accepted_record(
                &mut sibling,
                &proposal.record,
                original,
                q,
            ))
            .await;
        }
    }
    let remote_live = shop
        .app()
        .shared_record(&remote_key)
        .await
        .unwrap()
        .unwrap();
    let local_current = sibling
        .app()
        .shared_record(&local_key)
        .await
        .unwrap()
        .unwrap();
    assert!(local_current.version > old_delete.version);
    let mut wallet = Box::pin(Paykit::load(
        w.env(&w.alice, store),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: format!("records/{}", pubky_switch_core::enc::b64(&proposal.record)),
            after: 0,
        });
    wallet.app_mut().flush_message(&withdrawal).await.unwrap();
    assert!(matches!(
        wallet.app().delivery(&withdrawal).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    w.clock.advance_ms(60_000);
    wallet.flush().await.unwrap();
    wallet.app_mut().flush_message(&withdrawal).await.unwrap();
    let current = wallet
        .app()
        .shared_record(&local_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version, local_current.version);
    assert_eq!(current.content, local_current.content);
    assert!(!matches!(
        wallet.app().delivery(&withdrawal).await.unwrap(),
        Some(Delivery::Accepted { .. } | Delivery::Queued { .. })
    ));
    assert!(Box::pin(received_originals(&mut shop, &[&withdrawal]))
        .await
        .is_empty());
    let current = shop
        .app()
        .shared_record(&remote_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version, remote_live.version);
    assert_eq!(current.content, remote_live.content);
    if matches!(later, LaterProposal::Accepted) {
        for (app, key) in [(&wallet, &proposal.record), (&shop, &remote_public)] {
            let value = Box::pin(raw(app, key)).await;
            let accepted = AllowanceRecord::read(&value, &app.identity()).unwrap();
            assert!(accepted.response.is_some() && accepted.end.is_none());
        }
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_withdrawal_retries_the_exact_placed_proposal_on_cold_load() {
    Box::pin(withdraw_before_receiving(WithdrawalCase::Cold)).await;
}

enum WithdrawalCase {
    Together,
    Cold,
    Recreated,
}

async fn withdraw_before_receiving(case: WithdrawalCase) {
    use pubky_switch_client::app::{Collection, Delivery, RecordContent, RecordTarget};

    let w = World::new();
    let store = std::sync::Arc::new(pubky_switch_client::store::MemoryStore::new());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new()
                .allowance()
                .manage_contacts()
                .build()
                .unwrap(),
            Default::default(),
            store.clone(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new()
            .allowance()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .allowance()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let proposal = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    let family = paykit_lib::Collection::AllowanceProposals;
    let target = RecordTarget::new(
        Collection::new(family.name(), family.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &proposal.allowance_id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let original = if matches!(case, WithdrawalCase::Recreated) {
        wallet.receive(None).await.unwrap();
        let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
        Some(held.content.value().unwrap().to_vec())
    } else {
        None
    };
    let withdrawal = shop.end_allowance(&proposal.record).await.unwrap();
    assert!(matches!(
        shop.app().delivery(&withdrawal).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    let recreated = if let Some(value) = original.as_deref() {
        Box::pin(write_record(&mut phone, &target, None)).await;
        Box::pin(write_record(&mut phone, &target, Some(value))).await;
        wallet.app_mut().read_record_fresh(&target).await.unwrap();
        Some(
            wallet
                .app()
                .shared_record(&key)
                .await
                .unwrap()
                .unwrap()
                .version,
        )
    } else {
        None
    };
    if matches!(case, WithdrawalCase::Cold) {
        // Both signed messages are durable. Let the ordinary record creation apply and
        // interrupt the next service write before it answers.
        wallet.app_mut().receive(None).await.unwrap();
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unanswered {
                path: "/ops".into(),
                after: 1,
            });
        let _interrupted = wallet.receive(None).await;
        assert_eq!(w.record_writes(&w.alice, &key), 1);
        let first = wallet.app().shared_record(&key).await.unwrap().unwrap();
        assert!(first.authenticated && first.content.value().is_some());
        let public = wallet
            .app()
            .record_key(&allowances_target(&w.bob_id(), &proposal.allowance_id))
            .unwrap();
        assert_ne!(
            wallet.allowance(&public).await.unwrap().status,
            AllowanceStatus::Proposed
        );
        assert!(Box::pin(keeps(&wallet, b"ae/", &withdrawal)).await);
        let app_id = wallet.app_id();
        drop(wallet);
        wallet = Paykit::load(
            w.env(&w.alice, store),
            Default::default(),
            app_id,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
    }
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let current = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        current.authenticated
            && !current.doubtful
            && !current.divergent
            && !current.flagged
            && !current.not_taken
    );
    if let Some(version) = recreated {
        assert_eq!(current.version, version);
        assert_eq!(current.content.value(), original.as_deref());
    } else {
        assert_eq!(current.content, RecordContent::Deleted);
        assert_eq!(w.record_writes(&w.alice, &key), 2);
    }
    let permanent = wallet
        .app()
        .record_key(&allowances_target(&w.bob_id(), &proposal.allowance_id))
        .unwrap();
    assert!(wallet
        .app()
        .shared_record(&permanent)
        .await
        .unwrap()
        .is_none());
    assert!(wallet
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_pending_limit_uses_the_other_wallets_completed_creates_and_withdrawal() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.allowances().await.unwrap().is_empty());

    let mut proposals = Vec::new();
    for _ in 0..8 {
        let proposal = phone
            .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
            .await
            .unwrap();
        proposals.push(proposal);
    }
    shop.receive(None).await.unwrap();
    assert_eq!(shop.allowances().await.unwrap().len(), 8);
    let error = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::NotNow(_))),
        "{error:?}"
    );
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());
    phone.end_allowance(&proposals[0].record).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.allowances().await.unwrap().len(), 7);
    let replacement = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.allowances().await.unwrap();
    assert_eq!(held.len(), 8);
    assert!(held
        .iter()
        .any(|p| p.allowance_id == replacement.allowance_id));
    assert!(held
        .iter()
        .all(|p| p.allowance_id != proposals[0].allowance_id));
}

#[tokio::test]
async fn test_pending_capacity_requires_the_new_generation_after_provider_loss() {
    use pubky_switch_client::app::Collection;

    let w = World::new();
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let offer = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let kind = paykit_lib::Collection::AllowanceProposals;
    let family = Collection::new(kind.name(), kind.fixed_policy());
    let old = wallet.receive(None).await.unwrap().report;
    let before = wallet
        .app_mut()
        .current_collection(&family, &old)
        .await
        .unwrap()
        .unwrap();
    let pending = pending_key(&wallet, &w.bob_id(), &offer.allowance_id);
    assert_ne!(pending, offer.record);
    assert!(before.live.iter().any(|r| r.key == pending));

    crate::provider_loss::declare_loss(&w);
    wallet.app_mut().refresh_identity().await.unwrap();
    assert!(wallet
        .app_mut()
        .current_collection(&family, &old)
        .await
        .unwrap()
        .is_none());
    let held = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await;
    assert!(held.is_err(), "{held:?}");
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());

    Box::pin(crate::provider_loss::follow_loss(&mut wallet)).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut()
        .identity_of(&w.alice_id(), true)
        .await
        .unwrap();
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let now = wallet.receive(None).await.unwrap().report;
    let current = wallet
        .app_mut()
        .current_collection(&family, &now)
        .await
        .unwrap()
        .unwrap();
    assert!(current.through.generation > before.through.generation);
    assert!(current.uncertain_keys.is_empty());
    assert!(wallet
        .app_mut()
        .current_collection(&family, &old)
        .await
        .unwrap()
        .is_none());
    let next = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop
        .allowances()
        .await
        .unwrap()
        .iter()
        .any(|a| a.allowance_id == next.allowance_id));
}

#[tokio::test]
async fn test_pending_capacity_waits_for_a_fresh_listing_after_a_delayed_page() {
    use pubky_switch_client::{app::Collection, testing::Fault};

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let storage = crate::common::store();
    let scopes = AppScopes::new().allowance_execute().build().unwrap();
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        scopes.clone(),
        Default::default(),
        storage.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = Box::pin(alice_app(&w, "phone", scopes)).await;
    let first = phone
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let before = w.alice.switch().tip_seq();
    // The proposal is the same on both pages. A separate, ordinary invitation advances
    // the already verified account history while the earlier listing is in flight.
    phone.app_mut().invite(&w.carol_id()).await.unwrap();
    let complete = wallet.receive(None).await.unwrap().report;
    assert!(w.alice.switch().tip_seq() > before);
    let kind = paykit_lib::Collection::AllowanceProposals;
    let family = Collection::new(kind.name(), kind.fixed_policy());
    assert!(wallet
        .app_mut()
        .current_collection(&family, &complete)
        .await
        .unwrap()
        .is_some());
    let collection_id = pubky_switch_core::ident::collection_id(kind.name(), &wallet.identity());
    let page_path = format!(
        "records?collection={}",
        pubky_switch_core::enc::b64(&collection_id)
    );
    w.alice.switch().fail_next(Fault::Head {
        path: page_path.clone(),
        at: Some(before),
        forked: false,
    });
    assert!(wallet.app_mut().list_collections().await.unwrap());
    assert!(wallet
        .app_mut()
        .current_collection(&family, &complete)
        .await
        .unwrap()
        .is_none());
    // A missed entry makes the wallet relist before admitting another proposal.
    missed(&w);
    let unavailable = w.alice.switch().unreached_bodies().len();
    w.alice.switch().fail_next(Fault::Unreachable {
        path: page_path,
        after: 0,
    });
    let result = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(w.alice.switch().unreached_bodies().len(), unavailable + 1);
    // The interrupted listing requires the wallet to reload before inspecting its outbox.
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = Box::pin(w.load_on_clock(&w.alice, storage, app_id, w.clock.clone())).await;
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        0
    );

    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let added = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.allowances().await.unwrap();
    assert_eq!(held.len(), 2);
    assert!(held.iter().any(|a| a.allowance_id == first.allowance_id));
    assert!(held.iter().any(|a| a.allowance_id == added.allowance_id));
}

#[tokio::test]
async fn test_pending_capacity_waits_for_the_key_of_an_earlier_proposal() {
    use pubky_switch_client::app::{RecordContent, Unopened};

    let w = World::new();
    let (mut shop, mut wallet, mut phone) = Box::pin(two_wallets_with(
        &w,
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    wallet.receive(None).await.unwrap();
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let first = phone
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.allowances().await.unwrap().len(), 1);

    // This wallet holds the current epoch's key but missed the one used for the first proposal.
    w.alice.next_doc(|doc| doc.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &cert, newest);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    let held = wallet
        .app()
        .shared_record(&pending_key(&wallet, &w.bob_id(), &first.allowance_id))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        held.content,
        RecordContent::Unreadable(Unopened::KeyNotHeld { .. })
    ));
    let refusal = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(matches!(refusal.refusal(), Some(Refusal::NotNow(_))));
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        0
    );

    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let next = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let proposals = shop.allowances().await.unwrap();
    assert_eq!(proposals.len(), 2);
    for id in [first.allowance_id, next.allowance_id] {
        assert!(proposals.iter().any(|view| view.allowance_id == id));
    }
}

#[tokio::test]
async fn test_pending_capacity_resumes_from_a_listing_after_a_withdrawal_leaves_the_feed() {
    use pubky_switch_client::testing::{Fault, Retention};

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let mut original = Vec::new();
    for _ in 0..8 {
        original.push(
            phone
                .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
                .await
                .unwrap(),
        );
    }
    wallet.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(wallet.allowances().await.unwrap().len(), 8);
    assert_eq!(shop.allowances().await.unwrap().len(), 8);

    // The wallet misses the phone's two withdrawals while the service advances its checkpoint.
    let service = w.alice.switch();
    service.set_retention(Some(Retention {
        checkpoint_behind: 3,
        ..Retention::default()
    }));
    phone.end_allowance(&original[0].record).await.unwrap();
    let after_first_withdrawal = service.tip_seq();
    phone.end_allowance(&original[1].record).await.unwrap();
    phone.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.allowances().await.unwrap().len(), 6);
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    phone.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    phone.receive(None).await.unwrap();
    assert!(service.retained().1 >= after_first_withdrawal);
    assert!(wallet
        .app()
        .shared_record(&pending_key(
            &wallet,
            &w.bob_id(),
            &original[0].allowance_id
        ))
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .is_some());

    // A partial baseline cannot admit a new proposal. A later complete read can.
    service.fail_next(Fault::Unreachable {
        path: "records?collection".into(),
        after: 0,
    });
    assert!(wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .is_err());
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let replacement = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.allowances().await.unwrap();
    assert_eq!(held.len(), 7);
    assert!(held
        .iter()
        .any(|p| p.allowance_id == replacement.allowance_id));
}

/// Read the one payer choice produced by the public proposal or acceptance.
async fn original_approval(wallet: &Paykit, canonical: &[u8; 32]) -> Value {
    let key = format!("ao/{}", pubky_switch_core::enc::b64(canonical));
    let bytes = wallet
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["d"].clone()
}

#[tokio::test]
async fn test_original_payer_reopens_the_selected_proposal_and_sends_the_same_prepared_acceptance()
{
    use pubky_switch_client::app::KeptOp;
    use std::sync::Arc;
    let w = World::new();
    let store = Arc::new(crate::common::FailNextKeep::default());
    let mut payer = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        store.clone(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let (_, record) = Box::pin(proposed(&w, &mut shop, &mut payer)).await;
    let proposal = Box::pin(pending_at(&payer, &record)).await;
    let target = pending_target(&w.bob_id(), proposal.allowance_id());
    let (_, _, selected) = payer
        .app_mut()
        .read_record_application(&target)
        .await
        .unwrap()
        .unwrap();
    store.fail_before_keep();
    assert!(
        Box::pin(payer.accept_executable_allowance(&record, UserConfirmed::new()))
            .await
            .is_err()
    );
    let first = Box::pin(original_approval(&payer, &record)).await;
    assert_eq!(first["confirmed_for_execution"], true);
    assert_eq!(
        serde_json::from_value::<KeptOp>(first["opening"]["selected"].clone()).unwrap(),
        selected
    );
    assert!(first["opening"]["applied"].is_null());
    assert!(payer.app().shared_record(&record).await.unwrap().is_none());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_acceptance")).await,
        0
    );
    let app_id = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(Paykit::load(
        w.env(&w.alice, store),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    let id = Box::pin(payer.accept_allowance(&record)).await.unwrap();
    let after = Box::pin(original_approval(&payer, &record)).await;
    assert_eq!(
        after["opening"]["application"],
        first["opening"]["application"]
    );
    assert_eq!(after["decision"], first["decision"]);
    let writes: [KeptOp; 3] = serde_json::from_value(after["opening"]["applied"].clone()).unwrap();
    for (family, kept) in [
        paykit_lib::Collection::Allowances,
        paykit_lib::Collection::AllowanceLedgers,
        paykit_lib::Collection::AllowanceProposals,
    ]
    .into_iter()
    .zip(writes)
    {
        let target = pubky_switch_client::app::RecordTarget::new(
            pubky_switch_client::app::Collection::new(family.name(), family.fixed_policy()),
            &paykit_lib::derive::natural_key::allowance(&w.bob_id(), proposal.allowance_id()),
        );
        let (_, _, applied) = payer
            .app_mut()
            .read_record_application(&target)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept, applied);
    }
    let sent = payer.app().sent_to(&w.bob_id(), 0, u64::MAX).await.unwrap();
    assert_eq!(sent.iter().filter(|m| m.msg_id == id).count(), 1);
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_acceptance")).await,
        1
    );
}

/// The earlier stop was protected by a proposal still awaiting its first write.
/// Once the specific end is matched, only that end is exempt from the sender's bound.
#[tokio::test]
async fn test_matched_allowance_end_replaces_one_exemption_at_capacity() {
    for answer_first in [true, false] {
        Box::pin(replace_exempt_end(answer_first)).await;
    }
}

async fn replace_exempt_end(answer_first: bool) {
    let w = World::new();
    let _shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let (proposal, id) = unrecorded_proposal(&w, &wallet);
    let body = json!({ "allowance_id": id.as_str() });
    let owed = send(
        &w,
        &w.bob,
        "shop",
        &w.alice,
        wallet.app_id(),
        "paykit.allowance_proposal",
        vec![],
        json!({
            "allowance_id": id.as_str(), "proposer_role":"allowee", "allower_generation":1,
            "terms":terms().to_json().unwrap()
        }),
    );
    wallet.app_mut().receive(None).await.unwrap();
    assert!(Box::pin(keeps(&wallet, b"pr/", &owed)).await);
    // The earlier proposal still awaits placement at this wallet. A verified
    // retirement will let ordinary placement retire it before processing Q.
    Box::pin(records_proposal(&mut wallet, &proposal, &id)).await;
    let pending = pending_target(&w.bob_id(), &id);
    Box::pin(write_record(&mut wallet, &pending, None)).await;
    w.clock.advance_ms(1);
    let original_end = send(
        &w,
        &w.bob,
        "shop",
        &w.alice,
        wallet.app_id(),
        "paykit.allowance_end",
        vec![[0xee; 32], [0xdd; 32]],
        body.clone(),
    );
    let original_at = w.clock.now_ms();
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let mut other = Vec::new();
    for i in 0..bound {
        w.clock.advance_ms(1);
        other.push(end_from(&w, &w.bob, "shop", &wallet, i));
    }
    wallet.app_mut().receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound + 1);
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pd/")
        .await
        .unwrap()
        .is_empty());

    let record = Box::pin(records_proposal(&mut wallet, &proposal, &id)).await;
    let q = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.allowance_acceptance",
        vec![proposal.hash()],
        body.as_object().unwrap().clone(),
    );
    let end = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.allowance_end",
        vec![proposal.hash(), q.hash()],
        body.as_object().unwrap().clone(),
    );
    for m in if answer_first { [&q, &end] } else { [&end, &q] } {
        w.clock.advance_ms(1);
        w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], m);
    }
    wallet.app_mut().receive(None).await.unwrap();
    assert_eq!(Box::pin(stops(&wallet)).await, bound + 1);
    let marks = wallet.app().protocol_scan("paykit", b"pd/").await.unwrap();
    assert_eq!(marks.len(), 1);
    let mark: Value = serde_json::from_slice(&marks[0].1).unwrap();
    let evicted_at = if answer_first {
        original_at
    } else {
        original_at + 1
    };
    assert_eq!(mark["d"]["earliest_ms"], evicted_at);
    assert_eq!(mark["d"]["latest_ms"], evicted_at);
    assert_eq!(
        Box::pin(keeps(&wallet, b"ae/", &original_end)).await,
        !answer_first
    );
    assert_eq!(
        Box::pin(keeps(&wallet, b"ae/", &other[0])).await,
        answer_first
    );
    assert!(Box::pin(keeps(&wallet, b"ae/", other.last().unwrap())).await);
    let prefix = format!("ae/{}/", pubky_switch_core::enc::b64(&record));
    // Ordinary placement can now promote an end that arrived before Q.
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    let ends = wallet
        .app()
        .protocol_scan("paykit", prefix.as_bytes())
        .await
        .unwrap();
    assert!(
        ends.iter().any(|(_, v)| {
            let parsed: Value = serde_json::from_slice(v).unwrap();
            !parsed["d"]["ended_acceptance"].is_null()
        }),
        "matched end missing, answer_first={answer_first}"
    );
    assert_eq!(Box::pin(stops(&wallet)).await, bound + 1);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
}

#[tokio::test]
async fn test_counterparty_end_before_original_acceptance_placement() {
    Box::pin(counterparty_ends_its_acceptance(EndTiming::Before)).await;
}

#[tokio::test]
async fn test_counterparty_end_after_original_acceptance_placement() {
    Box::pin(counterparty_ends_its_acceptance(EndTiming::After)).await;
}

#[tokio::test]
async fn test_counterparty_end_absent_from_a_fresh_installation() {
    Box::pin(counterparty_ends_its_acceptance(EndTiming::FreshBackup)).await;
}

enum EndTiming {
    Before,
    After,
    FreshBackup,
}

async fn counterparty_ends_its_acceptance(timing: EndTiming) {
    let placed = matches!(timing, EndTiming::After);
    let w = World::new();
    let store = crate::common::store();
    let mut payer = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        store.clone(),
    ))
    .await;
    let mut seller = Box::pin(shop(&w)).await;
    let sent = payer
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    let before_answer = payer.backup().await.unwrap();
    seller.receive(None).await.unwrap();
    let peer = seller.allowances().await.unwrap().pop().unwrap();
    let answer = seller.accept_allowance(&peer.record).await.unwrap();
    if placed {
        let first = payer.receive(None).await.unwrap();
        assert!(received(&first, "paykit.allowance_acceptance"));
        assert_eq!(
            payer.allowance(&sent.record).await.unwrap().status,
            AllowanceStatus::Accepted
        );
    }
    let end = seller.end_allowance(&peer.record).await.unwrap();
    let mirror = AllowanceRecord::read(
        seller
            .app()
            .shared_record(&peer.record)
            .await
            .unwrap()
            .unwrap()
            .content
            .value()
            .unwrap(),
        &seller.identity(),
    )
    .unwrap();
    let q = mirror.response.unwrap();
    let e = mirror.end.unwrap();
    assert_eq!(q.message().msg_id, answer);
    assert_eq!(e.message().msg_id, end);
    assert_eq!(e.message().refs, [mirror.proposal.hash(), q.hash()]);
    let first = payer.receive(None).await.unwrap();
    if !placed {
        assert!(received(&first, "paykit.allowance_acceptance"), "{first:?}");
    }
    assert!(received(&first, "paykit.allowance_end"), "{first:?}");
    let view = payer.allowance(&sent.record).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Ended);
    assert!(!view.consented && view.executor_app_id.is_none());
    let app_id = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(Paykit::load(
        w.env(&w.alice, store.clone()),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..2 {
        payer.receive(None).await.unwrap();
    }
    let after = payer.allowance(&sent.record).await.unwrap();
    assert_eq!(after.status, AllowanceStatus::Ended);
    assert!(!after.consented && after.executor_app_id.is_none());
    if !placed {
        let notices = payer.notices().await.unwrap();
        assert_eq!(
            notices
                .iter()
                .filter(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                    && n.msg_id.as_deref() == Some(answer.as_str()))
                .count(),
            1
        );
        assert!(!Box::pin(keeps(&payer, b"pr/", &answer)).await);
        assert!(Box::pin(original_approval(&payer, &sent.record)).await["opening"].is_null());
        assert!(payer
            .app()
            .shared_record(&sent.record)
            .await
            .unwrap()
            .is_none());
        assert!(payer
            .app()
            .records_of("allowance_ledgers")
            .await
            .unwrap()
            .is_empty());
        assert!(w.rail.all_executions().is_empty());
        drop(payer);
        if matches!(timing, EndTiming::FreshBackup) {
            let blank = crate::common::store();
            let mut fresh = Box::pin(restore_pending_wallet(&w, blank, &before_answer)).await;
            Box::pin(crate::common::finish_listing(&mut fresh)).await;
            assert!(!Box::pin(keeps(&fresh, b"ae/", &end)).await);
            assert!(fresh
                .app()
                .records_of("allowance_ledgers")
                .await
                .unwrap()
                .is_empty());
            let view = fresh.allowance(&sent.record).await.unwrap();
            assert_eq!(view.status, AllowanceStatus::Proposed);
            assert!(!view.consented && view.executor_app_id.is_none());
            return;
        }
        payer = Box::pin(restore_pending_wallet(&w, store.clone(), &before_answer)).await;
        Box::pin(crate::common::finish_listing(&mut payer)).await;
        assert_eq!(
            payer.allowance(&sent.record).await.unwrap().status,
            AllowanceStatus::Ended,
            "the installed signed end must survive an older in-place import"
        );
        let with_end = payer.backup().await.unwrap();
        // A verified retirement ends this local stop. The same signed P on a
        // later lifetime has no accepted pair or execution authority.
        let mut phone = Box::pin(alice_app(
            &w,
            "phone",
            AppScopes::new().allowance().build().unwrap(),
        ))
        .await;
        let target = pending_target(&w.bob_id(), &sent.allowance_id);
        phone.app_mut().read_record_fresh(&target).await.unwrap();
        phone.end_allowance(&sent.record).await.unwrap();
        payer.app_mut().read_record_fresh(&target).await.unwrap();
        Box::pin(records_proposal(
            &mut phone,
            &mirror.proposal,
            &sent.allowance_id,
        ))
        .await;
        payer.app_mut().read_record_fresh(&target).await.unwrap();
        payer.receive(None).await.unwrap();
        let fresh = payer.allowance(&sent.record).await.unwrap();
        assert_eq!(fresh.status, AllowanceStatus::Proposed);
        assert!(!fresh.consented && fresh.executor_app_id.is_none());
        drop(payer);
        let mut restored = Box::pin(restore_pending_wallet(&w, store, &with_end)).await;
        Box::pin(crate::common::finish_listing(&mut restored)).await;
        assert_eq!(
            restored.allowance(&sent.record).await.unwrap().status,
            AllowanceStatus::Proposed
        );
        assert!(!Box::pin(keeps(&restored, b"ae/", &end)).await);
    } else {
        let held = Box::pin(value(&payer, &sent.record)).await;
        let accepted =
            AllowanceRecord::read(&serde_json::to_vec(&held).unwrap(), &payer.identity()).unwrap();
        assert_eq!(accepted.end.unwrap().hash(), e.hash());
    }
}

#[tokio::test]
async fn test_original_payer_recovers_its_incoming_acceptance_after_the_batch_applies() {
    for resume in [
        ResumeOriginal::Load,
        ResumeOriginal::Receive,
        ResumeOriginal::Flush,
    ] {
        Box::pin(recover_incoming_acceptance(resume)).await;
    }
}

enum ResumeOriginal {
    Load,
    Receive,
    Flush,
}

async fn recover_incoming_acceptance(resume: ResumeOriginal) {
    use pubky_switch_client::app::KeptOp;
    use pubky_switch_client::store::{Fault, FaultStore};
    use pubky_switch_core::{
        ops::{OpKind, SignedOp},
        records::AppCert,
    };
    use std::sync::Arc;

    let w = World::new();
    let disk = Arc::new(FaultStore::new(crate::common::store(), Fault::None));
    let mut payer = Box::pin(alice_on(
        &w,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        disk.clone(),
    ))
    .await;
    let mut seller = Box::pin(shop(&w)).await;
    let offer = payer
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    seller.receive(None).await.unwrap();
    let peer = seller.allowances().await.unwrap().pop().unwrap();
    seller.accept_allowance(&peer.record).await.unwrap();

    // Pause before native submission so the actual prepared approval sets the
    // store limit. The next receive can apply the batch but cannot save its proof.
    disk.refuse_puts_as_too_large(Some(b"opq/"));
    let _ = payer.receive(None).await;
    let at = format!("ao/{}", pubky_switch_core::enc::b64(&offer.record));
    let prepared = payer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let approval = Box::pin(original_approval(&payer, &offer.record)).await;
    assert!(approval["opening"].is_object() && approval["opening"]["applied"].is_null());
    assert!(payer
        .app()
        .shared_record(&offer.record)
        .await
        .unwrap()
        .is_none());
    assert!(payer
        .app()
        .protocol_scan("paykit", b"ap/")
        .await
        .unwrap()
        .is_empty());
    let id = payer.app_id();
    drop(payer);
    let load = || {
        Paykit::load(
            w.env(&w.alice, disk.clone()),
            Default::default(),
            id,
            w.rail.clone(),
            w.timer(),
        )
    };
    let mut payer = Box::pin(load()).await.unwrap();
    assert_eq!(
        Box::pin(original_approval(&payer, &offer.record)).await,
        approval
    );
    disk.refuse_puts_as_too_large_over(Some(b"p/paykit/ao/"), prepared.len());
    let failure = payer.receive(None).await.unwrap_err();
    assert!(matches!(
        failure,
        PaykitError::Client(pubky_switch_client::app::AppError::Store(
            pubky_switch_client::error::StoreError::TooLarge { .. }
        ))
    ));
    assert_eq!(
        Box::pin(original_approval(&payer, &offer.record)).await,
        approval
    );
    let mut applied = std::collections::BTreeMap::new();
    for member in payer.app().shared_evidence().await.unwrap().ops {
        let cert = AppCert::verify(&member.cert, &payer.identity()).unwrap();
        let op = SignedOp::verify(
            &member.op,
            &cert.signing_key(),
            &payer.identity(),
            Some(&cert.app_id()),
        )
        .unwrap();
        if op.body().batch.count != 3 || cert.app_id() != id {
            continue;
        }
        let OpKind::RecordPut(put) = &op.body().kind else {
            panic!("expected an original Allowance record operation");
        };
        assert!(member.proof.is_some());
        assert!(applied.insert(put.key, member).is_none());
    }
    let keys = [
        paykit_lib::Collection::Allowances,
        paykit_lib::Collection::AllowanceLedgers,
        paykit_lib::Collection::AllowanceProposals,
    ]
    .map(|family| {
        family
            .record_key(
                payer.app().naming_key(),
                &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &offer.allowance_id),
                None,
            )
            .unwrap()
    });
    let members = keys.map(|key| applied.remove(&key).unwrap());
    assert!(applied.is_empty());
    let ledger_key = keys[1];
    assert_eq!(w.record_writes(&w.alice, &ledger_key), 1);
    disk.refuse_puts_as_too_large(None);
    match resume {
        ResumeOriginal::Load => {
            drop(payer);
            payer = Box::pin(load()).await.unwrap();
        }
        ResumeOriginal::Receive => {
            payer.receive(None).await.unwrap();
        }
        ResumeOriginal::Flush => {
            payer.flush().await.unwrap();
        }
    }
    let captured = Box::pin(original_approval(&payer, &offer.record)).await;
    assert_eq!(captured["decision"], approval["decision"]);
    assert_eq!(
        serde_json::from_value::<[KeptOp; 3]>(captured["opening"]["applied"].clone()).unwrap(),
        members
    );
    payer.receive(None).await.unwrap();
    payer.flush().await.unwrap();
    assert_eq!(
        Box::pin(original_approval(&payer, &offer.record)).await,
        captured
    );
    assert_eq!(w.record_writes(&w.alice, &ledger_key), 1);
}

#[tokio::test]
async fn test_payer_keeps_the_budget_unopened_until_it_approves_automatic_payment() {
    let w = World::new();
    let mut payer = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let refused = payer
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms())
        .await
        .unwrap_err();
    assert!(matches!(refused.refusal(), Some(Refusal::Usage(_))));
    assert!(payer
        .app()
        .records_of("allowance_proposals")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_proposal")).await,
        0
    );

    let (_, record) = Box::pin(proposed(&w, &mut shop, &mut payer)).await;
    let original = Box::pin(pending_at(&payer, &record)).await;
    let refused = payer.accept_allowance(&record).await.unwrap_err();
    assert!(matches!(refused.refusal(), Some(Refusal::Usage(_))));
    assert_eq!(
        Box::pin(pending_at(&payer, &record))
            .await
            .proposal()
            .hash(),
        original.proposal().hash()
    );
    assert!(payer.app().shared_record(&record).await.unwrap().is_none());
    assert!(payer
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());
    assert!(payer
        .app()
        .protocol_scan("paykit", b"ao/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_acceptance")).await,
        0
    );

    let q = payer
        .accept_executable_allowance(&record, UserConfirmed::new())
        .await
        .unwrap();
    let held = Box::pin(original_approval(&payer, &record)).await;
    assert_eq!(held["confirmed_for_execution"], true);
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_acceptance")).await,
        1
    );
    assert!(payer
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .iter()
        .any(|m| m.msg_id == q));
}

#[tokio::test]
async fn test_pending_payer_without_ledger_write_scope_keeps_the_proposal_unanswered() {
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut payer = Box::pin(wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut payer)).await;
    let original = Box::pin(pending_at(&payer, &mine)).await;
    let error = Box::pin(payer.accept_executable_allowance(&mine, UserConfirmed::new()))
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(),Some(Refusal::ScopeMissing(scope)) if scope=="paykit_allowance_execute")
    );
    assert_eq!(
        Box::pin(pending_at(&payer, &mine)).await.proposal().hash(),
        original.proposal().hash()
    );
    assert!(payer.app().shared_record(&mine).await.unwrap().is_none());
    assert!(payer
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        Box::pin(taken(&mut shop, "paykit.allowance_acceptance")).await,
        0
    );
}

#[tokio::test]
async fn test_pending_reply_creates_the_accepted_pair_and_only_the_allowers_zero_ledger() {
    use paykit_lib::allowances::Ledger;
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, RecordContent, RecordTarget};
    let w = World::new();
    let mut bob = Box::pin(shop(&w)).await;
    let mut alice = Box::pin(alice_app(
        &w,
        "executor",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let sent = bob
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    let target = |collection: Collection, peer: [u8; 32]| {
        RecordTarget::new(
            SwitchCollection::new(collection.name(), collection.fixed_policy()),
            &paykit_lib::derive::natural_key::allowance(&peer, &sent.allowance_id),
        )
    };
    let p = Collection::AllowanceProposals;
    let a = Collection::Allowances;
    let l = Collection::AllowanceLedgers;
    let pending = target(p, w.bob_id());
    let pending_key = alice.app().record_key(&pending).unwrap();
    assert!(received(
        &alice.receive(None).await.unwrap(),
        "paykit.allowance_proposal"
    ));
    let original = alice
        .app()
        .shared_record(&pending_key)
        .await
        .unwrap()
        .unwrap();
    let original = paykit_lib::records::PendingAllowanceRecord::read(
        original.content.value().unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    let accepted_key = alice.app().record_key(&target(a, w.bob_id())).unwrap();
    let acceptance_id = alice
        .accept_executable_allowance(&accepted_key, UserConfirmed::new())
        .await
        .unwrap();
    let accepted = alice
        .app()
        .shared_record(&accepted_key)
        .await
        .unwrap()
        .unwrap();
    let accepted = AllowanceRecord::read(accepted.content.value().unwrap(), &w.alice_id()).unwrap();
    let q = accepted.response.as_ref().unwrap();
    assert_eq!(accepted.proposal.hash(), original.proposal().hash());
    assert_eq!(q.message().refs, [original.proposal().hash()]);
    assert_eq!(q.message().msg_id, acceptance_id);
    assert_eq!(q.message().from.app_id, alice.app().app_id());
    assert_eq!(
        alice
            .app()
            .shared_record(&pending_key)
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );
    let ledger_key = alice.app().record_key(&target(l, w.bob_id())).unwrap();
    let ledger = alice
        .app()
        .shared_record(&ledger_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ledger.content.value().unwrap(),
        Ledger::initial_value(&w.bob_id(), &sent.allowance_id, &q.hash())
    );
    let view = alice.allowance(&accepted_key).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Accepted);
    assert!(!view.consented && view.executor_app_id.is_none());
    assert!(received(
        &bob.receive(None).await.unwrap(),
        "paykit.allowance_acceptance"
    ));
    let mirror_key = bob.app().record_key(&target(a, w.alice_id())).unwrap();
    let mirror = bob.app().shared_record(&mirror_key).await.unwrap().unwrap();
    let mirror = AllowanceRecord::read(mirror.content.value().unwrap(), &w.bob_id()).unwrap();
    assert_eq!(mirror.proposal.hash(), original.proposal().hash());
    assert_eq!(mirror.response.unwrap().hash(), q.hash());
    assert!(bob
        .app()
        .shared_record(&bob.app().record_key(&target(l, w.alice_id())).unwrap())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_pending_reply_retry_uses_the_same_original_after_an_unanswered_acceptance() {
    use pubky_switch_client::app::Delivery;
    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let (_, mine) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let first = wallet
        .accept_executable_allowance(&mine, UserConfirmed::new())
        .await
        .unwrap();
    let kept = wallet.app().queued_messages("paykit").await.unwrap();
    let first_intent = kept.iter().find(|m| m.msg_id == first).unwrap();
    assert!(first_intent.kept_batch.is_some());
    assert!(!first_intent.may_send);
    let retry = wallet.accept_allowance(&mine).await.unwrap();
    assert_eq!(retry, first);
    w.clock.advance_ms(60_000);
    wallet.receive(None).await.unwrap();
    let delivery = wallet.app_mut().flush_message(&first).await.unwrap();
    assert!(
        matches!(delivery, Some(Delivery::Accepted { .. })),
        "delivery after settlement: {delivery:?}; queued: {:?}",
        wallet.app().queued_messages("paykit").await.unwrap()
    );
    assert!(received(
        &shop.receive(None).await.unwrap(),
        "paykit.allowance_acceptance"
    ));
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    let replies: Vec<_> = sent
        .iter()
        .filter(|m| m.kind == "paykit.allowance_acceptance")
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].msg_id, first);
}

#[tokio::test]
async fn test_pending_unanswered_acceptance_after_restore_observes_the_original_complete_result() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{
        Collection as SwitchCollection, KeptReason, RecordRead, RecordTarget,
    };
    use pubky_switch_core::chain::bind_answer;
    use pubky_switch_core::ops::{OpKind, SignedOp};
    use pubky_switch_core::records::AppCert;

    let w = World::new();
    let mut bob = Box::pin(shop(&w)).await;
    let mut alice = Box::pin(paying_wallet(&w)).await;
    let (_, pending) = Box::pin(proposed(&w, &mut bob, &mut alice)).await;
    let view = alice.allowance(&pending).await.unwrap();
    let target = |c: Collection| {
        RecordTarget::new(
            SwitchCollection::new(c.name(), c.fixed_policy()),
            &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &view.allowance_id),
        )
    };
    let (_, _, selected) = alice
        .app_mut()
        .read_record_application(&target(Collection::AllowanceProposals))
        .await
        .unwrap()
        .unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let original = alice
        .accept_executable_allowance(&pending, UserConfirmed::new())
        .await
        .unwrap();
    let queued = alice.app().queued_messages("paykit").await.unwrap();
    let batch = queued
        .iter()
        .find(|m| m.msg_id == original)
        .unwrap()
        .kept_batch
        .unwrap();
    let backup = alice.backup().await.unwrap();
    drop(alice);
    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    for _ in 0..3 {
        restored.receive(None).await.unwrap();
    }
    assert!(restored.app().kept_batch(batch).await.unwrap().is_none());
    assert!(matches!(
        restored.app_mut().flush_message(&original).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    let collections = [
        Collection::Allowances,
        Collection::AllowanceLedgers,
        Collection::AllowanceProposals,
    ];
    for c in collections {
        let read = restored
            .app_mut()
            .read_record_fresh(&target(c))
            .await
            .unwrap();
        match (&c, &read) {
            (_, RecordRead::Fresh(_)) => (),
            (
                Collection::AllowanceProposals,
                RecordRead::Kept {
                    reason: KeptReason::NotFound,
                    ..
                },
            ) => (),
            _ => panic!("{c:?}: {read:?}"),
        }
    }
    let held = Box::pin(original_approval(&restored, &pending)).await;
    assert_eq!(
        serde_json::from_value::<pubky_switch_client::app::KeptOp>(
            held["opening"]["selected"].clone()
        )
        .unwrap(),
        selected
    );
    assert_eq!(held["confirmed_for_execution"], true);
    let local: [pubky_switch_client::app::KeptOp; 3] =
        serde_json::from_value(held["opening"]["applied"].clone()).unwrap();
    let own = restored.identity();
    let app_id = restored.app_id();
    let keys: Vec<_> = collections
        .iter()
        .map(|c| restored.app().record_key(&target(*c)).unwrap())
        .collect();
    let shared = restored.app().shared_evidence().await.unwrap();
    let mut members = std::collections::BTreeMap::new();
    for held in shared.ops {
        let cert = AppCert::verify(&held.cert, &own).unwrap();
        if cert.app_id() != app_id {
            continue;
        }
        let op = SignedOp::verify(&held.op, &cert.signing_key(), &own, Some(&app_id)).unwrap();
        if op.body().batch.id != batch {
            continue;
        }
        assert_eq!(op.body().batch.count, 3);
        let OpKind::RecordPut(put) = &op.body().kind else {
            panic!("a record operation");
        };
        assert!(keys.contains(&put.key));
        let key = put.key;
        assert!(members
            .insert(
                key,
                (
                    op,
                    held.proof
                        .expect("the service proof for the original write")
                )
            )
            .is_none());
    }
    assert_eq!(
        members.len(),
        keys.len(),
        "the original two creates and pending delete"
    );
    for (key, kept) in keys.iter().zip(local) {
        let (op, proof) = members.get(key).unwrap();
        assert_eq!(&kept.op, op.signed());
        assert_eq!(kept.proof.as_ref(), Some(proof));
    }
    let (ops, proofs): (Vec<_>, Vec<_>) = keys.iter().map(|k| members.remove(k).unwrap()).unzip();
    let service_key = restored
        .app()
        .own_identity()
        .service_key_of(ops[0].body().generation)
        .unwrap();
    assert_eq!(
        bind_answer(&ops, &proofs, &service_key, &own)
            .unwrap()
            .len(),
        3
    );
    assert!(!restored.allowance(&keys[0]).await.unwrap().consented);
    let report = bob.receive(None).await.unwrap();
    assert!(received(&report, "paykit.allowance_acceptance"));
    let counterpart = bob
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.counterparty == own && a.allowance_id == view.allowance_id)
        .unwrap();
    assert_eq!(counterpart.status, AllowanceStatus::Accepted);
}

#[tokio::test]
async fn test_pending_reply_allower_proposer_creates_one_ledger_for_the_original_acceptance() {
    use paykit_lib::allowances::Ledger;

    let w = World::new();
    let mut wallet = Box::pin(paying_wallet(&w)).await;
    let mut shop = Box::pin(shop(&w)).await;
    let offer = wallet
        .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
        .await
        .unwrap();
    assert!(received(
        &shop.receive(None).await.unwrap(),
        "paykit.allowance_proposal"
    ));
    let offer_at_shop = shop
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.counterparty == w.alice_id() && a.allowance_id == offer.allowance_id)
        .unwrap();
    let reply = shop.accept_allowance(&offer_at_shop.record).await.unwrap();
    let mirror = shop
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let mirror = AllowanceRecord::read(mirror.content.value().unwrap(), &w.bob_id()).unwrap();
    let q = mirror.response.as_ref().unwrap();
    assert_eq!(q.message().msg_id, reply);
    assert_eq!(q.message().refs, [mirror.proposal.hash()]);
    assert_eq!(q.sender(), w.bob_id());
    assert_eq!(q.message().from.app_id, shop.app_id());
    assert!(shop
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());
    assert!(wallet
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());

    assert!(received(
        &wallet.receive(None).await.unwrap(),
        "paykit.allowance_acceptance"
    ));
    let accepted = wallet
        .app()
        .records_of("allowances")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let accepted = AllowanceRecord::read(accepted.content.value().unwrap(), &w.alice_id()).unwrap();
    assert_eq!(accepted.proposal.hash(), mirror.proposal.hash());
    assert_eq!(accepted.response.as_ref().unwrap().hash(), q.hash());
    let ledgers = wallet.app().records_of("allowance_ledgers").await.unwrap();
    let [ledger] = ledgers.as_slice() else {
        panic!("one initial ledger");
    };
    assert_eq!(
        ledger.content.value().unwrap(),
        Ledger::initial_value(&w.bob_id(), &offer.allowance_id, &q.hash())
    );
    assert_eq!(w.record_writes(&w.alice, &ledger.key), 1);
    shop.app_mut().resend(&reply).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &ledger.key), 1);
    assert!(wallet
        .allowances()
        .await
        .unwrap()
        .iter()
        .all(|a| a.status == AllowanceStatus::Accepted
            && !a.consented
            && a.executor_app_id.is_none()));
}

#[tokio::test]
async fn test_pending_rejection_delivers_from_its_applied_delete() {
    use pubky_switch_client::app::RecordContent;
    let w = World::new();
    let mut bob = Box::pin(shop(&w)).await;
    let mut alice = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let sent = bob
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    assert!(received(
        &alice.receive(None).await.unwrap(),
        "paykit.allowance_proposal"
    ));
    let key = pending_key(&alice, &w.bob_id(), &sent.allowance_id);
    let canonical = alice
        .app()
        .record_key(&allowances_target(&w.bob_id(), &sent.allowance_id))
        .unwrap();
    assert_ne!(canonical, key);
    let msg_id = alice.reject_allowance(&canonical).await.unwrap();
    assert!(matches!(
        alice.app().delivery(&msg_id).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    assert_eq!(
        alice
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );
    assert!(received(
        &bob.receive(None).await.unwrap(),
        "paykit.allowance_rejection"
    ));
    assert_eq!(
        bob.app()
            .shared_record(&pending_key(&bob, &w.alice_id(), &sent.allowance_id))
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );
    assert!(bob
        .app()
        .shared_record(&sent.record)
        .await
        .unwrap()
        .is_none());
    assert!(alice
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
    assert!(alice.allowances().await.unwrap().is_empty());
    assert!(bob.allowances().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_pending_rejection_waits_when_the_second_record_cannot_be_read() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, Delivery, RecordTarget};
    let w = World::new();
    let mut bob = Box::pin(shop(&w)).await;
    let mut alice = Box::pin(wallet(&w)).await;
    let (_, pending) = Box::pin(proposed(&w, &mut bob, &mut alice)).await;
    let view = alice.allowance(&pending).await.unwrap();
    let permanent = Collection::Allowances;
    let target = RecordTarget::new(
        SwitchCollection::new(permanent.name(), permanent.fixed_policy()),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &view.allowance_id),
    );
    let key = alice.app().record_key(&target).unwrap();
    // The preparation reads the permanent key once. Its next GET belongs to delivery.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: format!("records/{}", pubky_switch_core::enc::b64(&key)),
            after: 1,
        });
    let reply = alice.reject_allowance(&pending).await.unwrap();
    assert!(matches!(
        alice.app().delivery(&reply).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert_eq!(
        Box::pin(taken(&mut bob, "paykit.allowance_rejection")).await,
        0
    );
    alice.app_mut().flush_message(&reply).await.unwrap();
    assert!(matches!(
        alice.app().delivery(&reply).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    assert_eq!(
        Box::pin(taken(&mut bob, "paykit.allowance_rejection")).await,
        1
    );
}

#[tokio::test]
async fn test_pending_competing_proposal_retries_its_original_parked_entry_after_reload() {
    use pubky_switch_client::app::ParkReason;
    use std::sync::Arc;

    let w = World::new();
    let storage = Arc::new(crate::common::FailingRecordReads::default());
    let mut wallet = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
        Default::default(),
        storage.clone(),
    ))
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = Box::pin(shop(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let (_, canonical) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&canonical).await.unwrap().allowance_id;
    let physical = pending_key(&wallet, &w.bob_id(), &id);
    let writes = w.record_writes(&w.alice, &physical);
    Box::pin(read_listed_records(&mut wallet)).await;
    wallet.app_mut().flush_cursor().await.unwrap();
    let second = competing_proposal(&w, &shop, &wallet, &id);
    storage.limit_next_record(&physical);
    wallet.app_mut().receive(None).await.unwrap();
    let entries = wallet.app_mut().parked_entries().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].reason, ParkReason::StoreRejected);
    let parked = wallet
        .app_mut()
        .parked_entry(entries[0].stored_id)
        .await
        .unwrap()
        .unwrap();
    assert!(parked.sender_verified);
    assert_eq!(parked.sender, w.bob_id());
    assert_eq!(&parked.plaintext, second.plaintext());
    let me = wallet.app_id();
    drop(wallet);

    let mut reopened = Box::pin(w.load_on_clock(&w.alice, storage, me, w.clock.clone())).await;
    assert!(reopened
        .app_mut()
        .parked_entry(entries[0].stored_id)
        .await
        .unwrap()
        .is_some());
    reopened.receive(None).await.unwrap();
    assert!(reopened
        .app_mut()
        .parked_entries()
        .await
        .unwrap()
        .is_empty());
    let stops = reopened
        .app()
        .protocol_scan("paykit", b"pc/")
        .await
        .unwrap();
    assert_eq!(stops.len(), 1);
    let saved: Value = serde_json::from_slice(&stops[0].1).unwrap();
    let note = &saved["d"]["second"];
    assert_eq!(note["accepted_at"], parked.accepted_at_ms);
    assert_eq!(note["seq"], parked.seq);
    assert_eq!(note["generation"], parked.generation);
    assert_eq!(
        note["plaintext"],
        pubky_switch_core::enc::b64(&parked.plaintext.encode())
    );
    assert!(matches!(
        refusal(&reopened.reject_allowance(&canonical).await.unwrap_err()),
        Some(Refusal::AllowanceConflicted)
    ));
    reopened.receive(None).await.unwrap();
    assert!(w.record_writes(&w.alice, &physical) > writes);
    phone.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(pending_at(&phone, &canonical))
            .await
            .conflicted()
            .unwrap()
            .hash(),
        second.hash()
    );
}

/// The backup predates a deletion read by every then-enrolled app. A new
/// installation sees the service's current absence but did not witness the delete.
async fn restored_competitor_without_original(full: bool) {
    use pubky_switch_client::app::{KeptReason, RecordRead};

    let w = World::new();
    let mut shop = Box::pin(shop(&w)).await;
    let mut wallet = Box::pin(wallet(&w)).await;
    let mut phone = Box::pin(alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let (_, canonical) = Box::pin(proposed(&w, &mut shop, &mut wallet)).await;
    let id = wallet.allowance(&canonical).await.unwrap().allowance_id;
    let pending = pending_target(&w.bob_id(), &id);
    let accepted = allowances_target(&w.bob_id(), &id);
    let physical = wallet.app().record_key(&pending).unwrap();
    let second = competing_proposal(&w, &shop, &wallet, &id);
    let msg_id = &second.message().msg_id;
    wallet.app_mut().receive(None).await.unwrap();
    let owed = wallet.app().protocol_scan("paykit", b"pc/").await.unwrap();
    assert_eq!(owed.len(), 1);
    assert!(owed[0]
        .0
        .ends_with(pubky_switch_core::enc::b64(&physical).as_bytes()));
    let bytes: Value = serde_json::from_slice(&owed[0].1).unwrap();
    assert!(bytes["d"]["source"]["version"].as_u64().is_some());
    let backup = wallet.backup().await.unwrap();
    phone.receive(None).await.unwrap();
    assert!(matches!(
        phone.app_mut().read_record_fresh(&pending).await.unwrap(),
        RecordRead::Fresh(_)
    ));
    Box::pin(write_record(&mut phone, &pending, None)).await;
    for app in [&mut wallet, &mut phone] {
        app.receive(None).await.unwrap();
        app.app_mut().flush_cursor().await.unwrap();
    }
    assert!(matches!(
        phone.app_mut().read_record_fresh(&pending).await.unwrap(),
        RecordRead::Kept {
            reason: KeptReason::NotFound,
            ..
        }
    ));
    drop(wallet);
    if full {
        for _ in 0..8 {
            let other = paykit_lib::AllowanceId::generate().unwrap();
            propose_as(&w, &[phone.app_id()], other.as_str(), "0.20");
            phone.receive(None).await.unwrap();
        }
        assert_eq!(
            phone
                .allowances()
                .await
                .unwrap()
                .iter()
                .filter(|view| view.counterparty == w.bob_id()
                    && view.status == AllowanceStatus::Proposed)
                .count(),
            8
        );
    }
    let mut restored = Box::pin(restored_unreceived(&w, &backup)).await;
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"pc/")
            .await
            .unwrap(),
        owed
    );
    assert!(restored
        .app()
        .protocol_scan("paykit", b"pce/")
        .await
        .unwrap()
        .is_empty());
    for target in [&pending, &accepted] {
        let read = restored.app_mut().read_record_fresh(target).await.unwrap();
        assert!(
            matches!(
                read,
                RecordRead::Kept {
                    reason: KeptReason::NotFound,
                    record: None
                }
            ),
            "{read:?}"
        );
    }
    let before = w.record_writes(&w.alice, &physical);
    for _ in 0..3 {
        restored.receive(None).await.unwrap();
    }
    assert_eq!(
        w.record_writes(&w.alice, &physical),
        before,
        "a sourced second proposal cannot create an original"
    );
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"pc/")
            .await
            .unwrap(),
        owed,
        "admission must retain the original signed obligation"
    );
    assert!(!restored
        .notices()
        .await
        .unwrap()
        .iter()
        .any(
            |notice| notice.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && notice.msg_id.as_ref() == Some(msg_id)
        ));
    assert!(restored
        .app()
        .shared_record(&canonical)
        .await
        .unwrap()
        .is_none());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restored_competing_proposal_cannot_create_an_original_from_current_absence() {
    Box::pin(restored_competitor_without_original(false)).await;
}

#[tokio::test]
async fn test_restored_competing_proposal_remains_owed_at_full_admission() {
    Box::pin(restored_competitor_without_original(true)).await;
}

/// The actual signed answer the wallet kept just before its first batch could be stored.
async fn unsubmitted_answer(
    wallet: &mut Paykit,
    store: &crate::common::FailNextKeep,
    key: &[u8; 32],
    withdraw: bool,
) -> (String, [u8; 32]) {
    store.fail_before_keep();
    let result = if withdraw {
        wallet.end_allowance(key).await
    } else {
        wallet.reject_allowance(key).await
    };
    assert!(result.is_err());
    let (id, hash) = Box::pin(prepared_answer(wallet)).await;
    assert!(wallet.app().delivery(&id).await.unwrap().is_none());
    (id, hash)
}

async fn prepared_answer(wallet: &Paykit) -> (String, [u8; 32]) {
    use pubky_switch_core::{enc::unb64, message::MessagePlaintext};
    let prepared = wallet.app().protocol_scan("paykit", b"ap/").await.unwrap();
    let [(_, bytes)] = prepared.as_slice() else {
        panic!("one prepared answer")
    };
    let value: Value = serde_json::from_slice(bytes).unwrap();
    let original = MessagePlaintext::decode(
        &unb64(value["d"]["message"]["plaintext"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let message: Value = serde_json::from_slice(original.json()).unwrap();
    let msg_id = message["msg_id"].as_str().unwrap().to_owned();
    (msg_id, original.hash())
}

async fn resume_prepared_answer(withdraw: bool, restore_older: bool) {
    use pubky_switch_client::app::{Delivery, RecordContent};
    use std::sync::Arc;

    let w = World::new();
    let store = Arc::new(crate::common::FailNextKeep::default());
    let mut wallet = Box::pin(alice_on(
        &w,
        "original",
        AppScopes::new().allowance_execute().build().unwrap(),
        store.clone(),
    ))
    .await;
    let mut shop = Box::pin(shop(&w)).await;
    let record = if withdraw {
        let sent = wallet
            .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
            .await
            .unwrap();
        shop.receive(None).await.unwrap();
        sent.record
    } else {
        Box::pin(proposed(&w, &mut shop, &mut wallet)).await.1
    };
    let pending = Box::pin(pending_at(&wallet, &record)).await;
    let physical = pending_key(&wallet, &w.bob_id(), pending.allowance_id());
    let writes = w.record_writes(&w.alice, &physical);
    let first = Box::pin(unsubmitted_answer(&mut wallet, &store, &record, withdraw)).await;
    assert_eq!(w.record_writes(&w.alice, &physical), writes);
    let backup = wallet.backup().await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = Box::pin(Paykit::load(
        w.env(&w.alice, store.clone()),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    let retry = if withdraw {
        wallet.end_allowance(&record).await
    } else {
        wallet.reject_allowance(&record).await
    }
    .unwrap();
    assert_eq!(retry, first.0);
    assert!(matches!(
        wallet.app().delivery(&retry).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    assert_eq!(w.record_writes(&w.alice, &physical), writes + 1);
    assert_eq!(
        wallet
            .app()
            .shared_record(&physical)
            .await
            .unwrap()
            .unwrap()
            .content,
        RecordContent::Deleted
    );
    async fn delivered(wallet: &Paykit, peer: &[u8; 32], expected: &(String, [u8; 32])) {
        let sent = wallet.app().sent_to(peer, 0, u64::MAX).await.unwrap();
        let matching: Vec<_> = sent
            .iter()
            .filter(|item| item.msg_id == expected.0)
            .collect();
        let [item] = matching.as_slice() else {
            panic!("one delivery of the exact original")
        };
        assert_eq!(item.plaintext.hash(), expected.1);
        assert!(item.accepted_at_ms.is_some());
        assert!(item.resent_at_ms.is_empty());
    }
    Box::pin(delivered(&wallet, &w.bob_id(), &first)).await;
    if restore_older {
        w.clock.advance_ms(1);
        assert_eq!(
            Box::pin(records_proposal(
                &mut wallet,
                pending.proposal(),
                pending.allowance_id()
            ))
            .await,
            record
        );
        let next = Box::pin(unsubmitted_answer(&mut wallet, &store, &record, withdraw)).await;
        assert_ne!(next.0, first.0);
        assert_eq!(w.record_writes(&w.alice, &physical), writes + 2);
        drop(wallet);
        wallet = Box::pin(Paykit::restore(
            w.env(&w.alice, store.clone()),
            Default::default(),
            &backup,
            w.rail.clone(),
            w.timer(),
        ))
        .await
        .unwrap();
        crate::common::finish_listing(&mut wallet).await;
        let retry = if withdraw {
            wallet.end_allowance(&record).await
        } else {
            wallet.reject_allowance(&record).await
        }
        .unwrap();
        assert_eq!(retry, next.0);
        assert_eq!(w.record_writes(&w.alice, &physical), writes + 3);
        Box::pin(delivered(&wallet, &w.bob_id(), &next)).await;
    }
    assert!(wallet.app().shared_record(&record).await.unwrap().is_none());
    assert!(wallet
        .app()
        .records_of("allowance_ledgers")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_prepared_allowance_rejection_resumes_after_a_stop_before_the_batch() {
    Box::pin(resume_prepared_answer(false, false)).await;
}

#[tokio::test]
async fn test_prepared_allowance_withdrawal_resumes_after_a_stop_before_the_batch() {
    Box::pin(resume_prepared_answer(true, false)).await;
}

#[tokio::test]
async fn test_prepared_allowance_rejection_survives_an_older_in_place_import() {
    Box::pin(resume_prepared_answer(false, true)).await;
}

#[tokio::test]
async fn test_prepared_allowance_withdrawal_survives_an_older_in_place_import() {
    Box::pin(resume_prepared_answer(true, true)).await;
}

/// The same explicit decision can continue on renewal, but it cannot delete a later
/// incarnation created after an enrolled sibling closed the selected original.
async fn prepared_answer_after_change(withdraw: bool, renew_certificate: bool) {
    use pubky_switch_client::app::Delivery;
    use std::sync::Arc;

    let w = World::new();
    let store = Arc::new(crate::common::FailNextKeep::default());
    let scopes = AppScopes::new().allowance_execute().build().unwrap();
    let mut payer = Box::pin(alice_on(&w, "wallet", scopes.clone(), store.clone())).await;
    let mut peer = Box::pin(shop(&w)).await;
    let canonical = if withdraw {
        let sent = payer
            .propose_executable_allowance(w.bob_id(), terms(), UserConfirmed::new())
            .await
            .unwrap();
        peer.receive(None).await.unwrap();
        sent.record
    } else {
        Box::pin(proposed(&w, &mut peer, &mut payer)).await.1
    };
    let pending = Box::pin(pending_at(&payer, &canonical)).await;
    let target = pending_target(&w.bob_id(), pending.allowance_id());
    let key = payer.app().record_key(&target).unwrap();
    let before = w.record_writes(&w.alice, &key);
    let (original, hash) =
        Box::pin(unsubmitted_answer(&mut payer, &store, &canonical, withdraw)).await;
    assert_eq!(w.record_writes(&w.alice, &key), before);
    if renew_certificate {
        let id = payer.app_id();
        if !withdraw {
            let original_cert = payer.app().cert().clone();
            let receive_only = AppScopes::new().allowance().build().unwrap();
            let restricted = renew(&w, id, &receive_only, 1000);
            list(&w, id, &original_cert, &restricted);
            w.clock.advance_ms(2000);
            payer.receive(None).await.unwrap();
            assert_eq!(payer.app().cert().hash(), restricted.hash());
            assert!(payer.reject_allowance(&canonical).await.is_err());
            assert_eq!(w.record_writes(&w.alice, &key), before);
            assert!(payer.app().delivery(&original).await.unwrap().is_none());
        }
        let previous = payer.app().cert().clone();
        let renewed = renew(&w, id, &scopes, 1000);
        list(&w, id, &previous, &renewed);
        w.clock.advance_ms(2000);
        payer.receive(None).await.unwrap();
        assert_ne!(payer.app().cert().hash(), previous.hash());
        assert_eq!(payer.app().cert().signing_key(), previous.signing_key());
    } else {
        Box::pin(delete_pending_record(&w, pending.allowance_id())).await;
        let deleted = payer.app_mut().read_record_fresh(&target).await.unwrap();
        assert!(
            matches!(deleted,pubky_switch_client::app::RecordRead::Fresh(ref r) if r.content==pubky_switch_client::app::RecordContent::Deleted && r.authenticated && !r.divergent)
        );
        assert!(payer
            .app()
            .protocol_get(
                "paykit",
                format!("pce/{}", pubky_switch_core::enc::b64(&key)).as_bytes()
            )
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            Box::pin(records_proposal(
                &mut payer,
                pending.proposal(),
                pending.allowance_id()
            ))
            .await,
            canonical
        );
        assert_eq!(w.record_writes(&w.alice, &key), before + 2);
    }
    let app_id = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(Paykit::load(
        w.env(&w.alice, store),
        Default::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    if renew_certificate && !withdraw {
        // After the receive-only certificate, the renewed wallet still owes a listing.
        // Its first retry must preserve the signed rejection until the listing finishes.
        assert!(payer.app().listing_owed().await.unwrap());
        let prepared = payer.app().protocol_scan("paykit", b"ap/").await.unwrap();
        let error = payer.reject_allowance(&canonical).await.unwrap_err();
        assert!(
            matches!(error.refusal(), Some(Refusal::NotNow(reason)) if reason.contains("Call receive again")),
            "{error}"
        );
        assert_eq!(
            payer.app().protocol_scan("paykit", b"ap/").await.unwrap(),
            prepared
        );
        assert!(payer.app().delivery(&original).await.unwrap().is_none());
        assert_eq!(w.record_writes(&w.alice, &key), before);
        crate::common::finish_listing(&mut payer).await;
    }
    let id = if withdraw {
        payer.end_allowance(&canonical).await
    } else {
        payer.reject_allowance(&canonical).await
    }
    .unwrap();
    assert!(matches!(
        payer.app().delivery(&id).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    let sent = payer.app().sent_to(&w.bob_id(), 0, u64::MAX).await.unwrap();
    if renew_certificate {
        assert_eq!(id, original);
        assert_eq!(
            sent.iter()
                .find(|item| item.msg_id == id)
                .unwrap()
                .plaintext
                .hash(),
            hash
        );
        assert_eq!(w.record_writes(&w.alice, &key), before + 1);
    } else {
        assert_ne!(id, original);
        assert!(sent.iter().all(|item| item.msg_id != original));
        assert_eq!(w.record_writes(&w.alice, &key), before + 3);
        assert!(payer.app().delivery(&original).await.unwrap().is_none());
        // An outstanding earlier outcome cannot release a message merely because a
        // sibling deleted the old proposal. Its original replay window still applies.
        w.clock
            .advance_ms(pubky_switch_client::app::SENT_RETENTION_MS + 1);
        payer.receive(None).await.unwrap();
        assert!(payer
            .app()
            .protocol_scan("paykit", b"ap/")
            .await
            .unwrap()
            .is_empty());
        assert!(payer
            .notices()
            .await
            .unwrap()
            .iter()
            .any(|notice| notice.msg_id.as_deref() == Some(original.as_str())
                && notice.kind == paykit_sdk::NoticeKind::MessageGivenUp));
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_prepared_rejection_allows_an_answer_after_a_sibling_retires_its_proposal() {
    Box::pin(prepared_answer_after_change(false, false)).await;
}
#[tokio::test]
async fn test_prepared_withdrawal_allows_an_answer_after_a_sibling_retires_its_proposal() {
    Box::pin(prepared_answer_after_change(true, false)).await;
}
#[tokio::test]
async fn test_prepared_rejection_uses_its_original_signature_after_certificate_renewal() {
    Box::pin(prepared_answer_after_change(false, true)).await;
}
#[tokio::test]
async fn test_prepared_withdrawal_uses_its_original_signature_after_certificate_renewal() {
    Box::pin(prepared_answer_after_change(true, true)).await;
}
