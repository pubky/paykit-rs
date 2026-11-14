//! The ledger version a new executor builds on after a handoff (Paykit spec 8.3, "Handoff",
//! "Last ledger version"): the previous executor names its last ledger version in the
//! Allowance record's `ledger_handoff`, and a new executor that holds no ledger version builds
//! only on that write, or on one built on it or later, or on a ledger its user confirmed by
//! the repair.

use paykit_lib::grant::LedgerHandoff;
use paykit_lib::Decimal;
use paykit_sdk::{PayOutcome, Paykit};
use pubky_switch_client::app::{LocalWrite, RecordTarget, UserConfirmed};
use pubky_switch_core::ops::RecordVersion;
use serde_json::{json, Value};

use crate::common::World;
use crate::executor::{
    executor_wallet, held, refused_for, request, request_after_restore, restored_confirmed, set_up,
    terms_with,
};
use crate::ledger::is_rollback;

/// The local state key of the last ledger version `wallet` kept of the Allowance at `record`.
fn kept_key(record: &[u8; 32]) -> String {
    format!("lv/{}", pubky_switch_core::enc::b64(record))
}

/// An Allowance with a lifetime limit of 1 whose executor, the wallet, pays `paid` under it,
/// then hands it off to `wallet2`, which never read the ledger: it holds no ledger version.
/// The notice reaches the wallet. Returns the shop, the previous executor, the new one and
/// the Allowance record's key.
async fn handed_off(w: &World, paid: &[&str]) -> (Paykit, Paykit, Paykit, [u8; 32]) {
    let (mut shop, mut old, record) = Box::pin(set_up(
        w,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    ))
    .await;
    for value in paid {
        let request = request(w, &mut shop, &mut old, value).await;
        let outcome = Box::pin(old.pay_under_allowance(&request, &record))
            .await
            .unwrap();
        assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    }
    let mut new = Box::pin(executor_wallet(w, "wallet2")).await;
    new.receive(None).await.unwrap();
    Box::pin(new.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    new.app_mut()
        .protocol_commit(&[LocalWrite::Delete {
            key: kept_key(&record).into_bytes(),
        }])
        .await
        .unwrap();
    old.receive(None).await.unwrap();
    (shop, old, new, record)
}

fn d(amount: &str) -> Decimal {
    Decimal::parse(amount).unwrap()
}

#[tokio::test]
async fn test_handoff_new_executor_pays_on_the_write_named() {
    // Until the previous executor names its last ledger version, the new one pays nothing
    // automatically. Once the record holds the `ledger_handoff` and the switch shows that
    // write, it builds on it, and from then on keeps the write it built on.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let second = request(&w, &mut shop, &mut new, "0.5").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "hasn't named its last ledger version"),
        "{refused:?}"
    );
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let paid = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let (_, ledger) = held(&new, &record).await;
    assert_eq!(ledger.unwrap().entries.len(), 2);
    assert_eq!(new.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
    assert!(new.app().protocol_scan(b"lb/").await.unwrap().is_empty());
    // The lifetime limit counts the previous executor's payment.
    let third = request(&w, &mut shop, &mut new, "0.1").await;
    let refused = Box::pin(new.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_handoff_ledger_shown_older_than_the_write_named_pays_nothing() {
    // The previous executor names a ledger version later than any the switch shows: the
    // service shows the new executor an older ledger than the one named. It builds on none
    // of it: a rollback, which raises the alarm, and nothing is paid.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let key = kept_key(&record);
    let held_kept = old
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held_kept).unwrap();
    value["d"]["version"] = json!(value["d"]["version"].as_u64().unwrap() + 5);
    value["d"]["item_hash"] = json!(pubky_switch_core::enc::b64(&[7; 32]));
    old.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    Box::pin(old.resume_payments()).await.unwrap();
    let (allowance, _) = held(&old, &record).await;
    let named = allowance
        .ledger_handoff
        .expect("the previous executor names it");
    assert_eq!(named.item_hash, [7; 32]);
    new.receive(None).await.unwrap();
    let second = request(&w, &mut shop, &mut new, "0.5").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(is_rollback(&refused), "{refused:?}");
    assert!(new.ledger_alarm().is_some());
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_handoff_previous_executor_gone_pays_only_after_a_confirmed_repair() {
    // The previous executor never names its last ledger version, as when its app is removed
    // or its device lost: the new executor pays nothing automatically until its user
    // confirms the repair, which counts the previous executor's payment from its claim, and
    // then builds on the repaired write.
    let w = World::new();
    let (mut shop, _old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let second = request(&w, &mut shop, &mut new, "0.3").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "hasn't named its last ledger version"),
        "{refused:?}"
    );
    let repair = Box::pin(new.ledger_repair(&record)).await.unwrap();
    assert!(
        repair
            .reason
            .contains("hasn't named its last ledger version"),
        "{repair:?}"
    );
    assert_eq!(repair.lifetime, d("0.5"), "{repair:?}");
    assert!(w.rail.all_executions().len() == 1);
    Box::pin(new.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let third = request(&w, &mut shop, &mut new, "0.3").await;
    let refused = Box::pin(new.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
}

#[tokio::test]
async fn test_handoff_named_by_another_app_is_ignored_and_flagged() {
    // Another own app, never the executor, writes a `ledger_handoff` of its own into the
    // Allowance record, naming a ledger write for the new executor to build on. Its
    // signature isn't the previous executor's: the new executor's client flags the write and
    // keeps the copy it trusts, and nothing is paid on it.
    let w = World::new();
    let (mut shop, _old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    let keys = w.alice.identity().app_keys("other");
    let cert = w.alice.switch().app(&keys.app_id_bytes()).cert.unwrap();
    let grant = allowance.grants.last().unwrap().built_on;
    let forged = LedgerHandoff::sign(
        &keys,
        cert.signed().clone(),
        &w.alice_id(),
        &allowance.counterparty,
        &allowance.allowance_id,
        grant,
        RecordVersion {
            generation: grant.generation,
            version: 1,
        },
        [7; 32],
    );
    let next = paykit_lib::records::AllowanceRecord {
        ledger_handoff: Some(forged),
        ..allowance.clone()
    };
    let target = RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::collections::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    other.app_mut().read_record(&target).await.unwrap();
    other
        .app_mut()
        .put_record(&target, &next.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    new.receive(None).await.unwrap();
    assert!(new
        .app()
        .flagged_writes()
        .await
        .unwrap()
        .iter()
        .any(|flagged| flagged.key == record));
    let (trusted, _) = held(&new, &record).await;
    assert!(trusted.ledger_handoff.is_none());
    let second = request(&w, &mut shop, &mut new, "0.5").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "current write is one this app ignores"),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_handoff_restored_without_a_ledger_version_pays_only_through_the_repair() {
    // The record holds the previous executor's `ledger_handoff` for the new executor's
    // grant, but the new executor is restored from a backup that holds no ledger version:
    // writes it made after the named one may be lost with its state, so it builds only on a
    // ledger its user confirms by the repair.
    let w = World::new();
    let (mut shop, mut old, new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    Box::pin(old.resume_payments()).await.unwrap();
    let mut new = Box::pin(restored_confirmed(&w, new, &record)).await;
    assert!(held(&new, &record).await.0.ledger_handoff.is_some());
    let second = request_after_restore(&w, &mut shop, &mut new, "0.3").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "holds no ledger version to build on"),
        "{refused:?}"
    );
    let repair = Box::pin(new.ledger_repair(&record)).await.unwrap();
    assert_eq!(repair.lifetime, d("0.5"), "{repair:?}");
    Box::pin(new.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_handoff_previous_executor_names_its_last_version_and_attempts_nothing_after() {
    // At its next run after the handoff, the previous executor writes the `ledger_handoff`:
    // its last ledger version, for the grant that handed the Allowance on, signed by it. It
    // pays nothing under the Allowance from then on, and writes the record no more.
    let w = World::new();
    let (mut shop, mut old, _new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let kept: Value = serde_json::from_slice(
        &old.app()
            .protocol_get(kept_key(&record).as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    Box::pin(old.resume_payments()).await.unwrap();
    let (allowance, _) = held(&old, &record).await;
    let handoff = allowance.ledger_handoff.clone().expect("named");
    assert_eq!(handoff.app_id, old.app_id());
    assert_eq!(handoff.grant, allowance.grants[1].built_on);
    assert_eq!(
        handoff.ledger.version,
        kept["d"]["version"].as_u64().unwrap()
    );
    assert_eq!(
        pubky_switch_core::enc::b64(&handoff.item_hash),
        kept["d"]["item_hash"].as_str().unwrap()
    );
    assert_eq!(
        allowance.ledger_handoff_counts(&w.alice_id()).unwrap(),
        Some(&handoff)
    );
    let writes = w.record_writes(&w.alice, &record);
    let second = request(&w, &mut shop, &mut old, "0.2").await;
    let refused = Box::pin(old.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "another app is the Allowance's executor"),
        "{refused:?}"
    );
    Box::pin(old.resume_payments()).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &record), writes);
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// An Allowance its executor, the wallet, paid 0.5 under, handed off to `wallet2`, which
/// read the ledger before its grant and keeps that version, and pays 0.2 on it before the
/// previous executor names its last ledger version. Returns the shop, the previous
/// executor, the new one and the Allowance record's key.
async fn handed_off_to_a_reader(w: &World) -> (Paykit, Paykit, Paykit, [u8; 32]) {
    let (mut shop, mut old, record) = Box::pin(set_up(w, terms_with(json!({})))).await;
    let first = request(w, &mut shop, &mut old, "0.5").await;
    Box::pin(old.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    let mut new = Box::pin(executor_wallet(w, "wallet2")).await;
    new.receive(None).await.unwrap();
    Box::pin(new.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(new.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
    let second = request(w, &mut shop, &mut new, "0.2").await;
    let paid = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    old.receive(None).await.unwrap();
    (shop, old, new, record)
}

#[tokio::test]
async fn test_handoff_reader_executor_settles_on_the_write_its_first_write_built_on() {
    // A new executor that kept a version as a reader pays on it before the handoff is
    // named. The previous executor then names its last version, the one the new executor's
    // first write built on: the handoff settles, and payments go on.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off_to_a_reader(&w)).await;
    assert_eq!(new.app().protocol_scan(b"lb/").await.unwrap().len(), 1);
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let third = request(&w, &mut shop, &mut new, "0.2").await;
    let paid = Box::pin(new.pay_under_allowance(&third, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(new.app().protocol_scan(b"lb/").await.unwrap().is_empty());
    assert!(new.ledger_alarm().is_none());
}

#[tokio::test]
async fn test_handoff_reader_executor_built_on_an_older_version_than_named_is_a_rollback() {
    // The previous executor's last version is later than the one the new executor's first
    // write built on: it built on an older ledger than the previous executor's last, so the
    // named write raises the alarm, and nothing more is paid.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off_to_a_reader(&w)).await;
    let key = kept_key(&record);
    let held_kept = old
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held_kept).unwrap();
    value["d"]["version"] = json!(value["d"]["version"].as_u64().unwrap() + 5);
    value["d"]["item_hash"] = json!(pubky_switch_core::enc::b64(&[7; 32]));
    old.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let third = request(&w, &mut shop, &mut new, "0.2").await;
    let refused = Box::pin(new.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(is_rollback(&refused), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
}
