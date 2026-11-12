//! Allowances (Paykit spec 8.3): proposing, answering and ending, the `allowances` record on
//! either side, the kept end as the stop, the consent list and `executor_notice`.

use paykit_lib::allowances::AllowanceTerms;
use paykit_lib::grant::ExecutorGrant;
use paykit_lib::records::{AllowanceRecord, GrantEntry};
use paykit_sdk::{
    AllowanceRole, AllowanceStatus, AppScopes, Paykit, PaykitError, Refusal, RequestedScopes,
};
use pubky_switch_client::app::{AppEvent, LocalWrite, OpBatch};
use pubky_switch_client::clock::Clock;
use serde_json::{json, Value};

use crate::certificate::{is_not_now, list, renew, store_bundle};
use crate::common::{World, HOUR_MS};

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

/// Alice's wallet, holding `pay` and `allowance`.
async fn wallet(w: &World) -> Paykit {
    alice_app(
        w,
        "wallet",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await
}

/// Bob's shop, holding `request` and `allowance`; he admits alice.
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

#[tokio::test]
async fn test_allowances_proposal_accepted_on_both_sides() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Proposed);
    assert_eq!(view.role, AllowanceRole::Allower);
    assert!(!view.proposed_here);
    assert_eq!(view.counterparty, w.bob_id());
    assert_eq!(view.terms, terms());
    let sent = shop.allowance(&theirs).await.unwrap();
    assert_eq!(sent.role, AllowanceRole::Allowee);
    assert!(sent.proposed_here);
    assert_eq!(sent.allowance_id, view.allowance_id);
    wallet.accept_allowance(&mine).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    assert!(!value(&shop, &theirs).await["response"].is_null());
}

#[tokio::test]
async fn test_allowances_answer_refused_to_the_proposer_and_once_answered() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    // Only the proposal's recipient answers it.
    let error = shop.accept_allowance(&theirs).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    wallet.accept_allowance(&mine).await.unwrap();
    // Once.
    let error = wallet.reject_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    // An app without `allowance` proposes nothing.
    let mut plain = w.wallet("plain").await;
    let error = plain
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::ScopeMissing(_))),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_allowances_rejected_one_has_no_end() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.reject_allowance(&mine).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Rejected
    );
    for (app, record) in [(&mut shop, theirs), (&mut wallet, mine)] {
        let error = app.end_allowance(&record).await.unwrap_err();
        assert!(
            matches!(refusal(&error), Some(Refusal::Rejected)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn test_allowances_withdrawal_before_an_answer_ends_it() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    // The recipient doesn't end a proposal it hasn't answered: it rejects it.
    let error = wallet.end_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    shop.end_allowance(&theirs).await.unwrap();
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Ended
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(!value(&wallet, &mine).await["end"].is_null());
    let error = wallet.accept_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceEnded)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_allowances_counterparty_end_kept_as_the_stop_then_recorded() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
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
    let end = value(&wallet, &mine).await["end"].clone();
    assert!(!end.is_null());
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
    let error = wallet.end_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceEnded)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_allowances_end_naming_another_proposal_dropped() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
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
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_withdrawal_crossing_the_acceptance_ends_it_on_both_sides() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    // Alice accepts while bob withdraws: the two cross.
    wallet.accept_allowance(&mine).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    // The wallet's record holds the acceptance and can't hold the withdrawal: it ends the
    // Allowance itself, naming both, so the record shows it ended.
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    let end = value(&wallet, &mine).await["end"].clone();
    assert!(!end.is_null());
    // Bob gets the acceptance and alice's own end.
    let summary = shop.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.allowance_end"), "{summary:?}");
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Ended
    );
    // Bob's record keeps his withdrawal, the acceptance reflected only.
    let record = value(&shop, &theirs).await;
    assert!(record["response"].is_null(), "{record}");
}

#[tokio::test]
async fn test_allowances_end_for_a_record_not_listed_yet_is_held_until_listed() {
    // Alice's phone accepts bob's proposal; the wallet, holding `pay` only, can't read the
    // record. The wallet is renewed to read `allowance` too, with no bundle, and bob ends
    // the Allowance. The record's absence from the wallet's copy decides nothing: the end's
    // entry is held, the cursor before it, and no stop is kept from it as new state. Once
    // the keys come and the records are listed, the end is taken once and recorded.
    let w = World::new();
    let mut wallet = alice_app(&w, "wallet", AppScopes::new().pay().build().unwrap()).await;
    let me = wallet.app_id();
    let mut phone = alice_app(
        &w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
    let mut shop = shop(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut phone).await;
    phone.accept_allowance(&mine).await.unwrap();
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
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
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
    assert!(!value(&wallet, &mine).await["end"].is_null());
}

#[tokio::test]
async fn test_allowances_answer_before_its_record_kept_then_given_up() {
    // An acceptance of a proposal this identity's app holds no record of is kept, as new
    // state, not dropped; with no record coming, it goes 30 days after its acceptance.
    let w = World::new();
    let mut shop = shop(&w).await;
    let _wallet = wallet(&w).await;
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
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    assert!(shop.allowances().await.unwrap().is_empty());
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
}

/// Alice's executor app, holding `allowance_execute`, writes its grant into the accepted
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
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    consent(exec, record).await;
}

/// The consent list's entry of `record`, held and confirmed, as the executor's grant
/// writes it.
async fn consent(app: &mut Paykit, record: &[u8; 32]) {
    let key = format!("ac/{}", pubky_switch_core::enc::b64(record));
    app.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: key.into_bytes(),
            value: br#"{"v":1,"d":{"held":{"confirm_again":false}}}"#.to_vec(),
        }])
        .await
        .unwrap();
}

/// The consent list's entry of `record` at `app`, as stored.
async fn consent_entry(app: &Paykit, record: &[u8; 32]) -> Option<Value> {
    let key = format!("ac/{}", pubky_switch_core::enc::b64(record));
    app.app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .map(|v| serde_json::from_slice::<Value>(&v).unwrap()["d"].clone())
}

/// Bob's Allowance accepted by alice's wallet, with her `exec` app as its executor, and
/// her `phone`, another executor-capable app. Returns the apps and alice's record key.
async fn executed(w: &World) -> (Paykit, Paykit, Paykit, Paykit, [u8; 32], [u8; 32]) {
    let mut shop = shop(w).await;
    let mut wallet = wallet(w).await;
    let executor = AppScopes::new().allowance_execute().build().unwrap();
    let mut exec = alice_app(w, "exec", executor.clone()).await;
    let mut phone = alice_app(w, "phone", executor).await;
    let (theirs, mine) = proposed(w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    exec.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    grant(w, &mut exec, &mine).await;
    consent(&mut phone, &mine).await;
    wallet.receive(None).await.unwrap();
    let view = wallet.allowance(&mine).await.unwrap();
    assert_eq!(view.executor_app_id, Some(exec.app_id()));
    assert!(exec.allowance(&mine).await.unwrap().consented);
    (shop, wallet, exec, phone, theirs, mine)
}

#[tokio::test]
async fn test_allowances_end_notifies_the_executor_only() {
    let w = World::new();
    let (_shop, mut wallet, mut exec, mut phone, _, mine) = executed(&w).await;
    // The notice is wrapped to the executor the grant names, not to the phone.
    wallet.end_allowance(&mine).await.unwrap();
    let summary = exec.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.executor_notice"), "{summary:?}");
    assert_eq!(
        consent_entry(&exec, &mine).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    assert!(!exec.allowance(&mine).await.unwrap().consented);
    let summary = phone.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.executor_notice"), "{summary:?}");
    // The phone still drops it, for good, once it sees the record ended.
    assert_eq!(
        consent_entry(&phone, &mine).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_counterparty_end_recorded_with_a_notice() {
    let w = World::new();
    let (mut shop, mut wallet, mut exec, _phone, theirs, mine) = executed(&w).await;
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
    let (mut shop, _wallet, _exec, mut phone, theirs, mine) = executed(&w).await;
    phone.receive(None).await.unwrap();
    assert!(phone.allowance(&mine).await.unwrap().consented);
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    // The phone takes bob's end: its consent goes for good in the commit that keeps the stop.
    phone.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&phone, &mine).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
    assert!(!phone.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_restore_asks_consent_again_and_keeps_removals() {
    let w = World::new();
    let (_shop, mut wallet, mut exec, _phone, _, mine) = executed(&w).await;
    let backup = exec.backup().await.unwrap();
    let id = exec.app_id();
    drop(exec);
    let mut restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app_id(), id);
    restored.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&restored, &mine).await,
        Some(json!({ "held": { "confirm_again": true } }))
    );
    assert!(!restored.allowance(&mine).await.unwrap().consented);
    // A removal stays removed across a restore.
    wallet.end_allowance(&mine).await.unwrap();
    restored.receive(None).await.unwrap();
    let backup = restored.backup().await.unwrap();
    drop(restored);
    let mut again = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    again.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&again, &mine).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_app_without_allowance_takes_none_of_the_messages() {
    // An app whose certificate lacks `allowance` lists none of the kinds, so bob's proposal
    // goes only to the app that takes it, and the other writes no Allowance record.
    let w = World::new();
    let mut plain = w.wallet("plain").await;
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let summary = plain.receive(None).await.unwrap();
    assert!(
        !received(&summary, "paykit.allowance_proposal"),
        "{summary:?}"
    );
    assert!(plain.allowances().await.unwrap().is_empty());
    assert_eq!(w.record_writes(&w.alice, &mine), 1);
}

#[tokio::test]
async fn test_allowances_kept_stop_refuses_an_answer_before_it_is_recorded() {
    // Bob's end names an acceptance alice's record doesn't hold: it can't be recorded yet,
    // and is kept as the stop. Nothing is answered under it meanwhile.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
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
    assert_eq!(wallet.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    assert!(value(&wallet, &mine).await["end"].is_null());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    let error = wallet.accept_allowance(&mine).await.unwrap_err();
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
    let mut shop = shop(w).await;
    let mut wallet = wallet(w).await;
    let (_, mine) = proposed(w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    if ended {
        wallet.end_allowance(&mine).await.unwrap();
    }
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.receive(None).await.unwrap();
    restored
}

#[tokio::test]
async fn test_allowances_restore_asks_the_counterparty_of_an_open_one_to_resend() {
    // The backup may predate bob's end: he is asked to send again.
    let w = World::new();
    let restored = restored_with_allowance(&w, false).await;
    assert!(asked_bob(&w, &restored).await > 0);
}

#[tokio::test]
async fn test_allowances_restore_asks_nothing_for_an_ended_one() {
    let w = World::new();
    let restored = restored_with_allowance(&w, true).await;
    assert_eq!(asked_bob(&w, &restored).await, 0);
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
    let (_shop, _wallet, mut exec, _phone, _, mine) = executed(&w).await;
    notice(&w, &w.alice, "wallet", exec.app_id(), &mine, "handed_off");
    let summary = exec.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.executor_notice"), "{summary:?}");
    assert_eq!(
        consent_entry(&exec, &mine).await,
        Some(json!({ "removed": { "reason": "handed_off" } }))
    );
    let view = exec.allowance(&mine).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Accepted);
    assert!(!view.consented);
}

#[tokio::test]
async fn test_allowances_executor_notice_from_another_identity_dropped() {
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = executed(&w).await;
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
async fn test_allowances_stored_proposal_written_later() {
    // The receive step's write of bob's proposal is refused for good: the entry is stored,
    // and the wallet writes the proposal into its record by a later write of its own.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 2);
    wallet.receive(None).await.unwrap();
    assert!(wallet.allowances().await.unwrap().is_empty());
    wallet.receive(None).await.unwrap();
    let views = wallet.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
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
    let (theirs, mine) = proposed(w, shop, wallet).await;
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    phone.accept_allowance(&mine).await.unwrap();
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

/// A wallet and a phone of alice's, both holding `pay` and `allowance`, and bob's shop.
async fn two_wallets(w: &World) -> (Paykit, Paykit, Paykit) {
    let shop = shop(w).await;
    let wallet = wallet(w).await;
    let phone = alice_app(
        w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
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
    let (mut shop, mut wallet, mut phone) = two_wallets(&w).await;
    let (theirs, mine, cert) = sealed_allowance(&w, &mut shop, &mut wallet, &mut phone).await;
    shop.end_allowance(&theirs).await.unwrap();
    let got = wallet.receive(None).await.unwrap();
    assert!(got.key_epochs_awaited.is_empty(), "{got:?}");
    assert!(received(&got, "paykit.allowance_end"), "{got:?}");
    assert_eq!(wallet.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(wallet.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(!value(&wallet, &mine).await["end"].is_null());
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_end_kept_while_sealed_goes_once_it_opens_naming_another_proposal() {
    // An end kept at once over a sealed record is checked once the record opens: one that
    // refers to another proposal isn't this Allowance's end, and its stop goes, the Allowance
    // still accepted (Paykit wire 4.16).
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = two_wallets(&w).await;
    let (_, mine, cert) = sealed_allowance(&w, &mut shop, &mut wallet, &mut phone).await;
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
    assert_eq!(wallet.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    assert!(value(&wallet, &mine).await["end"].is_null());
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
        n += wallet.app().protocol_scan(prefix).await.unwrap().len();
    }
    n
}

/// Whether `wallet` keeps bob's end of the Allowance with ID number `i` as a stop.
async fn keeps_end(w: &World, wallet: &Paykit, i: usize) -> bool {
    let id =
        paykit_lib::AllowanceId::new(&format!("d0efffb7-191b-4709-b6c7-e343af55{i:04x}")).unwrap();
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let stop = format!("ae/{}", pubky_switch_core::enc::b64(&key));
    wallet
        .app()
        .protocol_get(stop.as_bytes())
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn test_allowances_stops_for_allowances_not_held_bounded_with_cancellations() {
    // Bob cancels half of 64 requests the wallet holds no record of and ends as many
    // Allowances it holds none of, a millisecond apart: the 64 stops are kept. One more end
    // drops the earliest of them, a cancellation, since the two kinds of stop count under one
    // bound per sender (Paykit spec 8.1, 8.3).
    let w = World::new();
    let _shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
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
    assert_eq!(stops(&wallet).await, all);
    w.clock.advance_ms(1);
    end_unknown(&w, &wallet, half);
    wallet.receive(None).await.unwrap();
    assert_eq!(stops(&wallet).await, all);
    assert_eq!(
        wallet.app().protocol_scan(b"kc/").await.unwrap().len(),
        half - 1
    );
    assert!(keeps_end(&w, &wallet, 0).await);
    assert!(keeps_end(&w, &wallet, half).await);
}

#[tokio::test]
async fn test_allowances_stop_for_an_allowance_held_never_dropped_for_the_bound() {
    // Bob's end of an Allowance the wallet holds names an acceptance its record doesn't hold:
    // the stop is kept, and counts in no bound. Then 64 ends of Allowances it holds no record
    // of, later: all are kept. A 65th drops the earliest of those, never the stop of the
    // Allowance held.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
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
    assert_eq!(stops(&wallet).await, 1);
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 0..bound {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(stops(&wallet).await, bound + 1);
    w.clock.advance_ms(1);
    end_unknown(&w, &wallet, bound);
    wallet.receive(None).await.unwrap();
    assert_eq!(stops(&wallet).await, bound + 1);
    let held = format!("ae/{}", pubky_switch_core::enc::b64(&mine));
    assert!(wallet
        .app()
        .protocol_get(held.as_bytes())
        .await
        .unwrap()
        .is_some());
    assert!(!keeps_end(&w, &wallet, 0).await);
    assert!(keeps_end(&w, &wallet, 1).await);
    assert!(keeps_end(&w, &wallet, bound).await);
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
    let (_shop, _wallet, mut exec, _phone, _, mine) = executed(&w).await;
    missed(&w);
    exec.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&exec, &mine).await,
        Some(json!({ "held": { "confirm_again": true } }))
    );
    assert!(!exec.allowance(&mine).await.unwrap().consented);
    consent(&mut exec, &mine).await;
    assert!(exec.allowance(&mine).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowances_missed_item_keeps_removals_and_consents_nothing_before_the_marks() {
    // A removal stays removed past a `missed` item. Until the receive marks each entry to be
    // confirmed again, the item's own mark consents nothing, as after a crash between them.
    let w = World::new();
    let (_shop, _wallet, mut exec, mut phone, _, mine) = executed(&w).await;
    notice(&w, &w.alice, "wallet", exec.app_id(), &mine, "handed_off");
    exec.receive(None).await.unwrap();
    missed(&w);
    exec.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&exec, &mine).await,
        Some(json!({ "removed": { "reason": "handed_off" } }))
    );
    assert!(phone.allowance(&mine).await.unwrap().consented);
    phone
        .app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: b"mc".to_vec(),
            value: br#"{"v":1,"d":true}"#.to_vec(),
        }])
        .await
        .unwrap();
    assert!(!phone.allowance(&mine).await.unwrap().consented);
    assert_eq!(
        consent_entry(&phone, &mine).await,
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
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    let day = pubky_switch_core::records::DAY_MS;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - day);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    w.clock.advance_ms(2 * day);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"se/").await.unwrap().is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, w.bob_id());
}
