//! Allowances (Paykit spec 8.3): proposing, answering and ending, the `allowances` record on
//! either side, the kept end as the stop, the consent list and `executor_notice`.

use paykit_lib::allowances::AllowanceTerms;
use paykit_lib::grant::ExecutorGrant;
use paykit_lib::records::{AllowanceRecord, GrantEntry};
use paykit_sdk::{
    AllowanceRole, AllowanceStatus, AppScopes, Paykit, PaykitError, Refusal, RequestedScopes,
};
use pubky_switch_client::app::{AppEvent, LocalWrite, OpBatch};
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
    wallet.receive(None).await.unwrap();
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
    shop.receive(None).await.unwrap();
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
