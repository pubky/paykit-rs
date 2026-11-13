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
        .protocol_commit(&[LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&entry).unwrap(),
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
        .map(|v| serde_json::from_slice::<Value>(&v).unwrap()["d"]["consent"].clone())
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

/// Whether `wallet` keeps bob's end of the Allowance with ID number `i` as a stop.
async fn keeps_end(w: &World, wallet: &Paykit, i: usize) -> bool {
    let target = allowance_record(w, i);
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
async fn test_allowances_stop_over_a_readable_record_counted_in_the_bound() {
    // Bob's end of an Allowance the wallet holds names an acceptance its record doesn't hold:
    // the stop is kept 30 days, under the bounds of the stops for records not held, counted
    // with them. Then 64 ends of Allowances it holds no record of, later: the 64th drops the
    // earliest of the sender's stops, the one over the record held (Paykit spec 8.1, 8.3).
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
    assert_eq!(stops(&wallet).await, bound);
    let held = format!("ae/{}", pubky_switch_core::enc::b64(&mine));
    assert!(wallet
        .app()
        .protocol_get(held.as_bytes())
        .await
        .unwrap()
        .is_none());
    assert!(keeps_end(&w, &wallet, 0).await);
    assert!(keeps_end(&w, &wallet, bound - 1).await);
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
            "proposer_role": "allowee",
            "terms": terms().to_json().unwrap(),
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
}

/// Bob ends an Allowance the wallet holds no record of, then, two hours on, sends its
/// proposal, whose write the switch refuses for good: the end is kept as the stop, and the
/// proposal as a stored entry's message, still to write. A read before the proposal lets
/// the app's own upkeep due by then go through, so the refusals land on the proposal's
/// writes alone.
async fn stop_with_a_stored_proposal(w: &World, wallet: &mut Paykit) {
    end_unknown(w, wallet, 0);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    propose_unknown(w, wallet, 0);
    refuse_writes(w, 2);
    wallet.receive(None).await.unwrap();
    assert!(keeps_end(w, wallet, 0).await);
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_allowances_stop_kept_past_30_days_while_a_stored_message_waits() {
    // The data epoch then rotates with no key for the wallet, so the proposal can't be
    // written yet. Past 30 days from the end's acceptance, the proposal is still within its
    // own 30 days, to write. The stop stays: once the proposal opens the record, the stop
    // must be there (Paykit spec 8.1, 8.3).
    let w = World::new();
    let _shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    stop_with_a_stored_proposal(&w, &mut wallet).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - HOUR_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
    assert!(keeps_end(&w, &wallet, 0).await);
}

#[tokio::test]
async fn test_allowances_stop_with_a_stored_message_never_dropped_for_the_bound() {
    // The stop of an Allowance whose stored proposal waits counts in no bound: 64 later ends
    // of other Allowances are all kept beside it, and a 65th drops the earliest of those.
    let w = World::new();
    let _shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    stop_with_a_stored_proposal(&w, &mut wallet).await;
    let bound = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for i in 1..=bound + 1 {
        w.clock.advance_ms(1);
        end_unknown(&w, &wallet, i);
    }
    refuse_writes(&w, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(stops(&wallet).await, bound + 1);
    assert!(keeps_end(&w, &wallet, 0).await);
    assert!(!keeps_end(&w, &wallet, 1).await);
    assert!(keeps_end(&w, &wallet, bound + 1).await);
}

/// Alice's wallet proposes an Allowance to bob, and a faulty own app, `tool`, writes a value
/// that doesn't read over its record. Alice's `phone` reads only now: it never saw the
/// record read, so it holds the value that doesn't read. Then bob accepts. Returns the phone,
/// the record key and the record's value as the wallet wrote it.
async fn answer_over_a_record_that_doesnt_read(w: &World) -> (Paykit, [u8; 32], Vec<u8>) {
    answer_after_a_value_that_doesnt_read(w, true).await
}

/// As [`answer_over_a_record_that_doesnt_read`]; with `phone_reads_first` false, the phone
/// reads only once bob has accepted, so the acceptance comes in the read that first holds
/// the record.
async fn answer_after_a_value_that_doesnt_read(
    w: &World,
    phone_reads_first: bool,
) -> (Paykit, [u8; 32], Vec<u8>) {
    let mut shop = shop(w).await;
    let mut wallet = wallet(w).await;
    let both = || AppScopes::new().pay().allowance().build().unwrap();
    let mut tool = alice_app(w, "tool", both()).await;
    let mut phone = alice_app(w, "phone", both()).await;
    let sent = wallet
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms())
        .await
        .unwrap();
    let written = serde_json::to_vec(&value(&wallet, &sent.record).await).unwrap();
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &sent.allowance_id),
    );
    tool.app_mut().read_record(&target).await.unwrap();
    let op = tool
        .app_mut()
        .record_op(
            &target,
            Some(b"{\"not\":\"an allowance record\"}"),
            Vec::new(),
        )
        .await
        .unwrap();
    tool.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    if phone_reads_first {
        phone.receive(None).await.unwrap();
        let held = phone
            .app()
            .shared_record(&sent.record)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            held.content.value(),
            Some(&b"{\"not\":\"an allowance record\"}"[..])
        );
    }
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.accept_allowance(&theirs).await.unwrap();
    (phone, sent.record, written)
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
    // The acceptance isn't dropped: it is kept as a stored entry, never written over the
    // value that doesn't read. Once an own app writes the record readable again, the phone
    // writes the acceptance in (Paykit spec 8.1, "Stored entries").
    let w = World::new();
    let (mut phone, record, written) = answer_over_a_record_that_doesnt_read(&w).await;
    let got = phone.receive(None).await.unwrap();
    assert!(stored_locally(&got), "{got:?}");
    assert!(!received(&got, "paykit.allowance_acceptance"), "{got:?}");
    assert_eq!(
        raw(&phone, &record).await,
        b"{\"not\":\"an allowance record\"}"
    );
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &w.bob_id(),
            &AllowanceRecord::read(&written, &w.alice_id())
                .unwrap()
                .allowance_id,
        ),
    );
    let op = phone
        .app_mut()
        .record_op(&target, Some(&written), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    w.clock.advance_ms(HOUR_MS);
    let mut taken = false;
    for _ in 0..3 {
        let got = phone.receive(None).await.unwrap();
        taken |= received(&got, "paykit.allowance_acceptance");
    }
    assert!(taken);
    assert_eq!(
        phone.allowance(&record).await.unwrap().status,
        AllowanceStatus::Accepted
    );
}

#[tokio::test]
async fn test_allowances_answer_over_a_record_that_doesnt_read_never_written_over_it() {
    // While the record doesn't read, the kept acceptance is never written over it; 30 days
    // from its acceptance it is given up, the host told (Paykit spec 8.1).
    let w = World::new();
    let (mut phone, record, _) = answer_over_a_record_that_doesnt_read(&w).await;
    phone.receive(None).await.unwrap();
    w.clock.advance_ms(HOUR_MS);
    phone.receive(None).await.unwrap();
    assert_eq!(
        raw(&phone, &record).await,
        b"{\"not\":\"an allowance record\"}"
    );
    assert!(phone.notices().await.unwrap().is_empty());
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    for _ in 0..2 {
        phone.receive(None).await.unwrap();
    }
    assert_eq!(
        raw(&phone, &record).await,
        b"{\"not\":\"an allowance record\"}"
    );
    let notices = phone.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, w.bob_id());
}

/// Alice's phone proposes an Allowance to bob; her wallet, which takes no message from bob,
/// reads its record. With `ended`, the phone withdraws it, and the wallet reads that too.
async fn allowance_seen_by_the_wallet(w: &World, ended: bool) -> Paykit {
    let _shop = shop(w).await;
    let mut phone = alice_app(
        w,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
    let mut wallet = wallet(w).await;
    let sent = phone
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms())
        .await
        .unwrap();
    if ended {
        phone.end_allowance(&sent.record).await.unwrap();
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.allowances().await.unwrap().len(), 1);
    assert_eq!(asked_bob(w, &wallet).await, 0);
    wallet
}

#[tokio::test]
async fn test_allowances_missed_item_asks_the_counterparty_of_an_open_one_to_resend() {
    // An end may be among the entries a `missed` item tells of: bob is asked to send again,
    // as for an open request (Paykit spec 8.3, 9).
    let w = World::new();
    let mut wallet = allowance_seen_by_the_wallet(&w, false).await;
    missed(&w);
    wallet.receive(None).await.unwrap();
    assert!(asked_bob(&w, &wallet).await > 0);
}

#[tokio::test]
async fn test_allowances_missed_item_asks_nothing_for_an_ended_one() {
    let w = World::new();
    let mut wallet = allowance_seen_by_the_wallet(&w, true).await;
    missed(&w);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked_bob(&w, &wallet).await, 0);
}

#[tokio::test]
async fn test_allowances_two_own_apps_answering_at_once_send_one_response() {
    // The wallet and the phone hold bob's proposal, and both accept it. The wallet's answer
    // is recorded first; the phone's write, built on the version it read, conflicts, and its
    // acceptance never goes out (Paykit spec 8.3): bob gets one response.
    let w = World::new();
    let (mut shop, mut wallet, mut phone) = two_wallets(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    wallet.accept_allowance(&mine).await.unwrap();
    let error = phone.accept_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    let summary = shop.receive(None).await.unwrap();
    let acceptances = summary
        .report
        .events
        .iter()
        .filter(|e| {
            matches!(e, AppEvent::MessageReceived { kind, .. } if kind == "paykit.allowance_acceptance")
        })
        .count();
    assert_eq!(acceptances, 1, "{summary:?}");
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Accepted
    );
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
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/inbox".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    let sent = wallet
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms())
        .await
        .unwrap();
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
    for _ in 0..2 {
        restored.receive(None).await.unwrap();
    }
    restored.flush().await.unwrap();
    let proposals = taken(&mut shop, "paykit.allowance_proposal").await
        + taken(&mut shop, "paykit.allowance_proposal").await;
    assert_eq!(proposals, 1);
    assert_eq!(w.record_writes(&w.alice, &sent.record), 1);
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
    let (_shop, mut wallet, _exec, mut phone, _, mine) = executed(&w).await;
    missed(&w);
    phone.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&phone, &mine).await,
        Some(json!({ "held": { "confirm_again": true } }))
    );
    wallet.end_allowance(&mine).await.unwrap();
    let got = phone.receive(None).await.unwrap();
    assert!(!received(&got, "paykit.allowance_end"), "{got:?}");
    assert!(!received(&got, "paykit.executor_notice"), "{got:?}");
    assert_eq!(
        consent_entry(&phone, &mine).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_proposal_whose_record_write_is_refused_is_never_sent() {
    // The proposal goes in the batch that creates its record: refused, nothing is sent
    // (Paykit spec 8.3, "Recorded first").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    refuse_writes(&w, 1);
    assert!(wallet
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms())
        .await
        .is_err());
    wallet.receive(None).await.unwrap();
    assert_eq!(taken(&mut shop, "paykit.allowance_proposal").await, 0);
    assert!(wallet.allowances().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_answer_whose_record_write_is_refused_is_never_sent() {
    // The acceptance is recorded at the version the wallet read, and sent only once that
    // write is applied: refused, nothing is sent and the Allowance stays proposed.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    refuse_writes(&w, 1);
    assert!(wallet.accept_allowance(&mine).await.is_err());
    wallet.receive(None).await.unwrap();
    assert_eq!(taken(&mut shop, "paykit.allowance_acceptance").await, 0);
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
        json!({ "allowance_id": allowance_id, "proposer_role": "allowee", "terms": terms })
            .as_object()
            .cloned()
            .unwrap(),
    );
}

#[tokio::test]
async fn test_allowances_second_proposal_conflicts_and_is_recorded() {
    // Bob proposes again under the same Allowance ID with other terms: the second proposal
    // is kept as the stop, then written into the record's `conflicted`. The Allowance shows
    // conflicted, and nothing is answered under it (Paykit spec 8.3, "Conflicted").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let writes = w.record_writes(&w.alice, &mine);
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    let got = wallet.receive(None).await.unwrap();
    assert!(received(&got, "paykit.allowance_proposal"), "{got:?}");
    // The step wrote nothing at the switch; the later write placed the conflict.
    assert_eq!(w.record_writes(&w.alice, &mine), writes + 1);
    assert!(wallet.app().protocol_scan(b"cf/").await.unwrap().is_empty());
    assert!(!value(&wallet, &mine).await["conflicted"].is_null());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    for error in [
        wallet.accept_allowance(&mine).await.unwrap_err(),
        wallet.reject_allowance(&mine).await.unwrap_err(),
    ] {
        assert!(
            matches!(refusal(&error), Some(Refusal::AllowanceConflicted)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn test_allowances_second_response_conflicts_and_nothing_is_sent() {
    // Alice's wallet accepts; a rejection of hers comes to bob too, another response with
    // another hash: bob's Allowance is conflicted, and his shop sends no end.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    shop.receive(None).await.unwrap();
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&shop, &theirs).await).unwrap(),
        &w.bob_id(),
    )
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.allowance_rejection",
        vec![record.proposal.hash()],
        json!({ "allowance_id": record.allowance_id.as_str() })
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
    assert_eq!(taken(&mut wallet, "paykit.allowance_end").await, 0);
}

#[tokio::test]
async fn test_allowances_second_end_is_reflected_not_conflicted() {
    // A second, different end is processed but not stored: the Allowance is ended, never
    // conflicted (Paykit spec 8.3).
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
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
    let held = value(&wallet, &mine).await;
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
    let (_shop, _wallet, mut exec, _phone, _, mine) = executed(&w).await;
    let id = exec.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[exec.app_id()], id.as_str(), "0.20");
    exec.receive(None).await.unwrap();
    assert_eq!(
        consent_entry(&exec, &mine).await,
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
    let (mut shop, mut wallet, mut phone) = two_wallets(&w).await;
    let (_, mine, cert) = sealed_allowance(&w, &mut shop, &mut wallet, &mut phone).await;
    let id = phone.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    let got = wallet.receive(None).await.unwrap();
    assert!(!received(&got, "paykit.allowance_proposal"), "{got:?}");
    assert!(wallet.app().protocol_scan(b"cf/").await.unwrap().is_empty());
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
    let (mut shop, mut wallet, mut phone) = two_wallets(&w).await;
    let (_, mine, _) = sealed_allowance(&w, &mut shop, &mut wallet, &mut phone).await;
    let error = wallet.accept_allowance(&mine).await.unwrap_err();
    assert!(is_not_now(&error), "{error:?}");
    let listed = wallet.allowances().await.unwrap_err();
    assert!(is_not_now(&listed), "{listed:?}");
}

#[tokio::test]
async fn test_allowances_end_over_a_rejected_record_is_reflected() {
    // Alice rejects while bob withdraws: her record holds the rejection, and bob's
    // withdrawal is reflected by its `msg_id`, the Allowance still rejected (Paykit wire
    // 4.16).
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.reject_allowance(&mine).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    let withdrawal = AllowanceRecord::read(
        &serde_json::to_vec(&value(&shop, &theirs).await).unwrap(),
        &w.bob_id(),
    )
    .unwrap()
    .end
    .unwrap()
    .message()
    .msg_id
    .clone();
    wallet.receive(None).await.unwrap();
    let held = value(&wallet, &mine).await;
    assert!(
        held["msg_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &json!(withdrawal)),
        "{held}"
    );
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Rejected
    );
}

#[tokio::test]
async fn test_allowances_crossing_end_reflected_by_the_withdrawer() {
    // Bob withdraws while alice accepts; alice ends naming both. Bob's record holds his
    // withdrawal, and reflects alice's end by its `msg_id`.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    let own_end = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap()
    .end
    .unwrap()
    .message()
    .msg_id
    .clone();
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    let held = value(&shop, &theirs).await;
    assert!(
        held["msg_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &json!(own_end)),
        "{held}"
    );
}

#[tokio::test]
async fn test_allowances_end_naming_another_acceptance_kept_30_days_and_consent_kept() {
    // Bob's end names an acceptance other than the one the record holds: it is kept as the
    // stop 30 days, which stops payment meanwhile, and the phone's consent entry stays, since
    // the end may yet prove not to be this Allowance's. Past 30 days the stop goes, and the
    // consent counts again.
    let w = World::new();
    let (_shop, mut wallet, _exec, mut phone, _, mine) = executed(&w).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
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
    assert_eq!(phone.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    assert_eq!(
        consent_entry(&phone, &mine).await,
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
    assert!(phone.app().protocol_scan(b"ae/").await.unwrap().is_empty());
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
    let (_shop, _wallet, mut exec, _phone, _, _) = executed(&w).await;
    notice(&w, &w.alice, "wallet", exec.app_id(), &[5; 32], "ended");
    exec.receive(None).await.unwrap();
    assert_eq!(consent_entry(&exec, &[5; 32]).await, None);
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
    let _shop = shop(&w).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_allowances_kept_answer_over_a_record_that_doesnt_read_given_up_with_a_notice() {
    // The acceptance comes before the phone holds the record, so it is kept; the record then
    // comes as a value that doesn't read. The answer is never written over it, and 30 days
    // from its acceptance it is given up, the host told (Paykit spec 8.1).
    let w = World::new();
    let (mut phone, record, _) = answer_after_a_value_that_doesnt_read(&w, false).await;
    phone.receive(None).await.unwrap();
    assert_eq!(phone.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    phone.receive(None).await.unwrap();
    assert_eq!(phone.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    assert!(phone.notices().await.unwrap().is_empty());
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    phone.receive(None).await.unwrap();
    assert!(phone.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    assert_eq!(
        raw(&phone, &record).await,
        b"{\"not\":\"an allowance record\"}"
    );
    let notices = phone.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
}

/// A `missed` item, and Paykit's listing at the end of the next receive fails: the copy of
/// the records counts as not listed until a listing of Paykit's own.
fn missed_and_listing_fails(w: &World) {
    missed(w);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "records?collection".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
}

#[tokio::test]
async fn test_allowances_stop_past_30_days_kept_while_unlisted() {
    // A stop for an Allowance the wallet holds no record of is past its 30 days when the copy
    // of the records may lack records: the absence decides nothing, so it stays until a
    // listing of Paykit's own, and goes then (Paykit spec 8.1).
    let w = World::new();
    let _shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    assert!(keeps_end(&w, &wallet, 0).await);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    missed_and_listing_fails(&w);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert!(keeps_end(&w, &wallet, 0).await);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(!keeps_end(&w, &wallet, 0).await);
}

#[tokio::test]
async fn test_allowances_kept_answer_past_30_days_kept_while_unlisted() {
    // The same for an answer kept before its record.
    let w = World::new();
    let mut shop = shop(&w).await;
    let _wallet = wallet(&w).await;
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
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    w.bob.switch().push_missed(1, now - 1_000, now);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "records?collection".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_stored_proposal_neither_opened_nor_given_up_while_unlisted() {
    // Bob's proposal is stored; past 30 days the copy may lack records: the proposal opens
    // no record from that copy, and isn't given up, until a listing of Paykit's own.
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
    missed_and_listing_fails(&w);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(wallet.app().protocol_scan(b"se/").await.unwrap().len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.allowances().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_allowances_end_removes_the_consent_though_its_record_write_fails() {
    // The phone takes bob's end and removes its consent entry for good in the commit that
    // keeps the stop; its later write of the end into the record is refused, so the record
    // doesn't show it yet, and the entry stays removed.
    let w = World::new();
    let (mut shop, _wallet, _exec, mut phone, theirs, mine) = executed(&w).await;
    shop.receive(None).await.unwrap();
    shop.end_allowance(&theirs).await.unwrap();
    refuse_writes(&w, 1);
    phone.receive(None).await.unwrap();
    assert!(value(&phone, &mine).await["end"].is_null());
    assert_eq!(
        consent_entry(&phone, &mine).await,
        Some(json!({ "removed": { "reason": "ended" } }))
    );
}

#[tokio::test]
async fn test_allowances_stop_under_another_record_key_is_corrupt() {
    // A kept end read back under the key of another record than the one it names is
    // corrupt state, never taken as a stop or as none (Paykit spec 8.3).
    let w = World::new();
    let _shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    end_unknown(&w, &wallet, 0);
    wallet.receive(None).await.unwrap();
    let (key, value) = wallet.app().protocol_scan(b"ae/").await.unwrap().remove(0);
    let other = format!("ae/{}", pubky_switch_core::enc::b64(&[3; 32]));
    wallet
        .app_mut()
        .protocol_commit(&[
            LocalWrite::Delete { key },
            LocalWrite::Put {
                key: other.into_bytes(),
                value,
            },
        ])
        .await
        .unwrap();
    let error = wallet.receive(None).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

#[tokio::test]
async fn test_allowances_missed_mark_other_than_true_is_corrupt() {
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = executed(&w).await;
    exec.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: b"mc".to_vec(),
            value: br#"{"v":1,"d":false}"#.to_vec(),
        }])
        .await
        .unwrap();
    let error = exec.allowance(&mine).await.unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error:?}");
}

#[tokio::test]
async fn test_allowances_consent_under_a_key_not_its_spelling_is_corrupt() {
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, _) = executed(&w).await;
    exec.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: b"ac/not-a-record-key".to_vec(),
            value: br#"{"v":1,"d":{"held":{"confirm_again":false}}}"#.to_vec(),
        }])
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
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    refuse_writes(&w, 2);
    let got = wallet.receive(None).await.unwrap();
    assert!(value(&wallet, &mine).await["conflicted"].is_null());
    assert_eq!(wallet.app().protocol_scan(b"cf/").await.unwrap().len(), 1);
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
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
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
    assert_eq!(stops(&wallet).await, bound);
    assert!(!keeps_end(&w, &wallet, 0).await);
    let held = format!("ae/{}", pubky_switch_core::enc::b64(&mine));
    assert!(wallet
        .app()
        .protocol_get(held.as_bytes())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_allowances_stop_over_a_record_holding_what_it_names_never_dropped_for_the_bound() {
    // Bob withdraws the Allowance the wallet holds, then ends 64 it holds no record of, in
    // one read. The withdrawal's stop is over a record that reads and holds the proposal it
    // names: it is the stop of a held Allowance, outside the bounds of the stops for records
    // not held, so the 64 later ones push none of it out, and the later write records it
    // (Paykit spec 8.3, "Ends reach the executor").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let record = AllowanceRecord::read(
        &serde_json::to_vec(&value(&wallet, &mine).await).unwrap(),
        &w.alice_id(),
    )
    .unwrap();
    w.clock.advance_ms(1);
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_end",
        vec![record.proposal.hash()],
        json!({ "allowance_id": record.allowance_id.as_str() })
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
    assert!(!value(&wallet, &mine).await["end"].is_null());
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert_eq!(stops(&wallet).await, bound);
    assert!(keeps_end(&w, &wallet, 0).await);
}

#[tokio::test]
async fn test_allowances_kept_answer_refused_past_30_days_given_up_with_a_notice() {
    // The acceptance is kept before the phone holds a record that reads. Past 30 days from
    // its acceptance the record reads, and the switch refuses for good the write placing the
    // answer: that refusal is the record's, so the answer is given up, the host told, as a
    // payer's kept reply is (Paykit spec 8.1).
    let w = World::new();
    let (mut phone, record, written) = answer_after_a_value_that_doesnt_read(&w, false).await;
    phone.receive(None).await.unwrap();
    assert_eq!(phone.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &w.bob_id(),
            &AllowanceRecord::read(&written, &w.alice_id())
                .unwrap()
                .allowance_id,
        ),
    );
    let op = phone
        .app_mut()
        .record_op(&target, Some(&written), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    // The app's own renewals first, so the refusal meets the write placing the answer.
    phone.app_mut().rotate().await.unwrap();
    phone.app_mut().flush_cursor().await.unwrap();
    refuse_writes(&w, 1);
    phone.receive(None).await.unwrap();
    assert!(phone.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    let notices = phone.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(raw(&phone, &record).await, written);
}

#[tokio::test]
async fn test_allowances_consent_under_another_records_key_is_corrupt() {
    // A consent entry read back under the key of another Allowance's record than the one it
    // names is a corrupt store: it would let the executor pay under an Allowance its user
    // never consented to (Paykit spec 8.3).
    let w = World::new();
    let (_shop, _wallet, mut exec, _phone, _, mine) = executed(&w).await;
    let key = format!("ac/{}", pubky_switch_core::enc::b64(&mine));
    let value = exec
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let other = format!("ac/{}", pubky_switch_core::enc::b64(&[3; 32]));
    exec.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: other.into_bytes(),
            value,
        }])
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
    let (_shop, mut wallet, _exec, _phone, _, mine) = executed(&w).await;
    wallet.end_allowance(&mine).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"en/").await.unwrap().len(), 1);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"en/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_allowances_notice_target_naming_another_notice_is_corrupt() {
    // A notice target read back under another notice's `msg_id` than the one it names is a
    // corrupt store, returned by the receive, never a silent "send to no one".
    let w = World::new();
    let (_shop, mut wallet, _exec, _phone, _, mine) = executed(&w).await;
    wallet.end_allowance(&mine).await.unwrap();
    let (key, value) = wallet.app().protocol_scan(b"en/").await.unwrap().remove(0);
    wallet
        .app_mut()
        .protocol_commit(&[
            LocalWrite::Delete { key },
            LocalWrite::Put {
                key: b"en/123e4567-e89b-42d3-a456-426614174000".to_vec(),
                value,
            },
        ])
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
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let status = if ended {
        wallet.accept_allowance(&mine).await.unwrap();
        wallet.end_allowance(&mine).await.unwrap();
        AllowanceStatus::Ended
    } else {
        wallet.reject_allowance(&mine).await.unwrap();
        AllowanceStatus::Rejected
    };
    let mut restored = restored_and_resent(&w, &mut shop, wallet).await;
    assert_eq!(restored.allowance(&mine).await.unwrap().status, status);
    assert!(restored.accept_allowance(&mine).await.is_err());
    assert!(restored.reject_allowance(&mine).await.is_err());
}

#[tokio::test]
async fn test_allowances_ended_never_revived_by_a_restore_and_resends() {
    terminal_through_a_restore(true).await;
}

#[tokio::test]
async fn test_allowances_rejected_never_revived_by_a_restore_and_resends() {
    terminal_through_a_restore(false).await;
}

#[tokio::test]
async fn test_allowances_conflicted_kept_through_a_restore_and_resends() {
    // A conflicted Allowance, the wallet restored from a backup and bob's messages sent to it
    // again: it stays conflicted, the record keeps `conflicted`, and nothing is answered
    // (Paykit spec 8.3, "Never cleared").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    propose_as(&w, &[wallet.app_id()], id.as_str(), "0.20");
    wallet.receive(None).await.unwrap();
    assert!(!value(&wallet, &mine).await["conflicted"].is_null());
    let mut restored = restored_and_resent(&w, &mut shop, wallet).await;
    assert_eq!(
        restored.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    assert!(!value(&restored, &mine).await["conflicted"].is_null());
    let error = restored.accept_allowance(&mine).await.unwrap_err();
    assert!(
        matches!(refusal(&error), Some(Refusal::AllowanceConflicted)),
        "{error:?}"
    );
}

/// Alice's tool app deletes her record of the Allowance with ID `id`, as a faulty own app
/// could: no app of Paykit's deletes one.
async fn delete_allowance_record(w: &World, id: &paykit_lib::AllowanceId) {
    let mut tool = alice_app(
        w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), id),
    );
    tool.app_mut().read_record(&target).await.unwrap();
    let op = tool
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    tool.app_mut()
        .submit_batch(OpBatch::new().entry(op))
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
async fn test_allowances_proposal_over_a_deleted_record_is_conflicted_after_a_restore() {
    // An accepted Allowance's record is deleted by another own app. The wallet is restored
    // from a backup taken before the delete, bob sends his messages again, and then proposes
    // again under the same Allowance ID: the app can't tell what the deleted record held, so
    // the proposal is taken as conflicted, kept as the stop, and nothing is written at the
    // key; the acceptance and its limits never come back (Paykit spec 8.3, "Ending").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    delete_allowance_record(&w, &id).await;
    let mut restored = restored_and_resent(&w, &mut shop, wallet).await;
    w.clock.advance_ms(HOUR_MS);
    propose_as(&w, &[restored.app_id()], id.as_str(), "0.10");
    restored.receive(None).await.unwrap();
    assert!(held_deleted(&restored, &mine).await);
    assert_eq!(restored.app().protocol_scan(b"cf/").await.unwrap().len(), 1);
    assert!(restored.accept_allowance(&mine).await.is_err());
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
async fn test_allowances_proposal_over_a_deleted_record_is_conflicted_at_a_second_app() {
    // The phone held the record when another own app deleted it; bob then proposes again
    // under the same Allowance ID. The phone takes the proposal as conflicted and writes
    // nothing at the key, and its executor consent goes for good (Paykit spec 8.3).
    let w = World::new();
    let (_shop, _wallet, _exec, mut phone, _, mine) = executed(&w).await;
    let id = phone.allowance(&mine).await.unwrap().allowance_id;
    delete_allowance_record(&w, &id).await;
    phone.receive(None).await.unwrap();
    assert!(held_deleted(&phone, &mine).await);
    w.clock.advance_ms(HOUR_MS);
    propose_as(&w, &[phone.app_id()], id.as_str(), "0.10");
    phone.receive(None).await.unwrap();
    assert!(held_deleted(&phone, &mine).await);
    assert_eq!(phone.app().protocol_scan(b"cf/").await.unwrap().len(), 1);
    assert_eq!(
        consent_entry(&phone, &mine).await,
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
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    shop.end_allowance(&theirs).await.unwrap();
    refuse_writes(&w, 2);
    wallet.receive(None).await.unwrap();
    assert!(value(&wallet, &mine).await["end"].is_null());
    assert_eq!(wallet.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    let mut restored = restored_and_resent(&w, &mut shop, wallet).await;
    assert_eq!(
        restored.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
    assert!(restored.accept_allowance(&mine).await.is_err());
}

/// An end over a record the wallet's copy doesn't hold yet, held while unlisted past the
/// client's bounded wait: the client stores the entry for the store, Paykit notes it, and
/// the end must still be taken once the records are listed.
#[tokio::test]
async fn test_allowances_end_held_past_the_bounded_wait_still_taken() {
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
    let ends = wallet.app().protocol_scan(b"ae/").await.unwrap().len();
    let end = value(&wallet, &mine).await["end"].clone();
    assert!(
        status == AllowanceStatus::Ended,
        "status {status:?}, ae/ {ends}, end {end}, parked {parked}"
    );
}
/// A delete is taken as any delete; a later write that brings the record back with the
/// proposal alone drops the acceptance the wallet saw, so the wallet ignores it.
#[tokio::test]
async fn test_allowances_write_back_after_a_delete_without_what_was_seen_ignored() {
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let proposed_only = serde_json::to_vec(&value(&wallet, &mine).await).unwrap();
    wallet.accept_allowance(&mine).await.unwrap();
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let mut tool = alice_app(
        &w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(&w.bob_id(), &id),
    );
    tool.app_mut().read_record(&target).await.unwrap();
    let op = tool
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    tool.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.app_mut().read_record(&target).await.unwrap();
    assert!(held_deleted(&wallet, &mine).await);
    tool.app_mut().read_record(&target).await.unwrap();
    let op = tool
        .app_mut()
        .record_op(&target, Some(&proposed_only), Vec::new())
        .await
        .unwrap();
    tool.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.app_mut().read_record(&target).await.unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&mine).await.unwrap().unwrap();
    assert!(
        held.flagged && held.content == pubky_switch_client::app::RecordContent::Deleted,
        "write back taken: flagged {}, value {:?}",
        held.flagged,
        held.content
            .value()
            .map(|v| String::from_utf8_lossy(v).into_owned())
    );
    let accept = wallet.accept_allowance(&mine).await;
    assert!(accept.is_err(), "accepted again: {accept:?}");
}

/// The `allowances` record at `target` written by `app` to `value`, `None` for a delete,
/// as a faulty own app could.
async fn write_record(
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
        .submit_batch(OpBatch::new().entry(op))
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
async fn test_allowances_stored_proposal_over_a_record_deleted_since_is_conflicted() {
    // The wallet's write of bob's proposal is refused for good, so the entry is stored. Another
    // own app then writes the record and deletes it, so the wallet holds it as deleted, never
    // having seen it. The later write finds the record held as deleted: the proposal is kept
    // as the conflict's stop and nothing is written at the key (Paykit spec 8.3, "Ending").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms())
        .await
        .unwrap();
    refuse_writes(&w, 2);
    let got = wallet.receive(None).await.unwrap();
    assert!(stored_locally(&got), "{got:?}");
    let target = allowances_target(&w.bob_id(), &sent.allowance_id);
    let mine = wallet.app().record_key(&target).unwrap();
    let mut tool = alice_app(
        &w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
    write_record(
        &mut tool,
        &target,
        Some(b"{\"not\":\"an allowance record\"}"),
    )
    .await;
    write_record(&mut tool, &target, None).await;
    let writes = w.record_writes(&w.alice, &mine);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert!(held_deleted(&wallet, &mine).await);
    assert_eq!(wallet.app().protocol_scan(b"cf/").await.unwrap().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &mine), writes);
    let views = wallet.allowances().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].status, AllowanceStatus::Conflicted);
}

#[tokio::test]
async fn test_allowances_end_over_a_deleted_record_kept_as_a_stop_once_listed() {
    // Another own app deletes the record of an accepted Allowance. Bob's end then comes while
    // the copy of the records isn't listed: the delete decides nothing, so the end is held;
    // once listed, the end is kept as the stop for an Allowance whose record the app doesn't
    // hold, and nothing is answered under it (Paykit spec 8.3, "Ending").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    wallet.accept_allowance(&mine).await.unwrap();
    shop.receive(None).await.unwrap();
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    delete_allowance_record(&w, &id).await;
    wallet.receive(None).await.unwrap();
    assert!(held_deleted(&wallet, &mine).await);
    missed_and_listing_fails(&w);
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"ae/").await.unwrap().is_empty());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"ae/").await.unwrap().len(), 1);
    assert!(wallet.end_allowance(&mine).await.is_err());
}

#[tokio::test]
async fn test_allowances_answer_over_a_deleted_record_kept_before_its_record() {
    // Bob's tool app deletes his record of the Allowance he proposed; alice's acceptance then
    // comes. Over a record held as deleted it is kept as a reply before its record, under
    // those bounds, and the record isn't written (Paykit spec 8.3, "Ending").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = wallet(&w).await;
    let (theirs, mine) = proposed(&w, &mut shop, &mut wallet).await;
    let id = wallet.allowance(&mine).await.unwrap().allowance_id;
    let mut tool = w
        .join(
            &w.bob,
            "tool",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    write_record(&mut tool, &allowances_target(&w.alice_id(), &id), None).await;
    shop.receive(None).await.unwrap();
    assert!(held_deleted(&shop, &theirs).await);
    wallet.accept_allowance(&mine).await.unwrap();
    let writes = w.record_writes(&w.bob, &theirs);
    shop.receive(None).await.unwrap();
    assert!(held_deleted(&shop, &theirs).await);
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    assert_eq!(w.record_writes(&w.bob, &theirs), writes);
}

#[tokio::test]
async fn test_allowances_consent_paused_under_an_ignored_current_write() {
    // Another own app writes the accepted Allowance's record without its acceptance: the
    // executor-capable phone ignores that write, as it saw the acceptance, and its consent
    // no longer counts while that write is current, though its entry stays (Paykit spec 8.3,
    // "Executor").
    let w = World::new();
    let (_shop, _wallet, _exec, mut phone, _, mine) = executed(&w).await;
    assert!(phone.allowance(&mine).await.unwrap().consented);
    let id = phone.allowance(&mine).await.unwrap().allowance_id;
    let mut record = value(&phone, &mine).await;
    record.as_object_mut().unwrap().remove("response");
    let mut tool = alice_app(
        &w,
        "tool",
        AppScopes::new().pay().allowance().build().unwrap(),
    )
    .await;
    write_record(
        &mut tool,
        &allowances_target(&w.bob_id(), &id),
        Some(&serde_json::to_vec(&record).unwrap()),
    )
    .await;
    phone.receive(None).await.unwrap();
    let held = phone.app().shared_record(&mine).await.unwrap().unwrap();
    assert!(held.flagged);
    assert!(!value(&phone, &mine).await["response"].is_null());
    assert!(!phone.allowance(&mine).await.unwrap().consented);
    assert_eq!(
        consent_entry(&phone, &mine).await,
        Some(json!({ "held": { "confirm_again": false } }))
    );
}

#[tokio::test]
async fn test_allowances_view_shows_a_blocked_counterparty() {
    // The wallet blocks bob: his Allowance shows blocked, as his requests do, and unblocked
    // once he is (Paykit spec 8.1, "Blocked counterparties").
    let w = World::new();
    let mut shop = shop(&w).await;
    let mut wallet = alice_app(
        &w,
        "keeper",
        AppScopes::new()
            .pay()
            .allowance()
            .manage_contacts()
            .build()
            .unwrap(),
    )
    .await;
    let (_, mine) = proposed(&w, &mut shop, &mut wallet).await;
    assert!(!wallet.allowance(&mine).await.unwrap().blocked);
    wallet.block(&w.bob_id()).await.unwrap();
    assert!(wallet.allowance(&mine).await.unwrap().blocked);
    assert!(wallet.allowances().await.unwrap()[0].blocked);
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert!(!wallet.allowance(&mine).await.unwrap().blocked);
}
