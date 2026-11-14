//! The ledger version a new executor builds on after a handoff (Paykit spec 8.3, "Handoff",
//! "Last ledger version"): the previous executor names its last ledger version in the
//! Allowance record's `ledger_handoff`, and a new executor that holds no ledger version builds
//! only on that write, or on one built on it or later, or on a ledger its user confirmed by
//! the repair.

use paykit_lib::grant::LedgerHandoff;
use paykit_lib::Decimal;
use paykit_sdk::{PayOutcome, Paykit};
use pubky_switch_client::app::{LocalWrite, RecordTarget, UserConfirmed};
use pubky_switch_client::clock::Clock;
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
/// read the ledger before its grant and keeps that version as a reader. The notice reaches
/// the wallet. Returns the shop, the previous executor, the new one and the Allowance
/// record's key.
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
    old.receive(None).await.unwrap();
    (shop, old, new, record)
}

/// The previous executor's kept version moved, later by 5 when `later`, else earlier by 1,
/// with another item hash when `other_item`, so its `ledger_handoff` names that version.
async fn name_another_version(old: &mut Paykit, record: &[u8; 32], by: i64, other_item: bool) {
    let key = kept_key(record);
    let held_kept = old
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held_kept).unwrap();
    let version = value["d"]["version"].as_u64().unwrap();
    value["d"]["version"] = json!(version.checked_add_signed(by).unwrap());
    if other_item {
        value["d"]["item_hash"] = json!(pubky_switch_core::enc::b64(&[7; 32]));
    }
    old.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
}

/// The new executor's user confirms the repair while the handoff is unsettled.
async fn repaired(new: &mut Paykit, record: &[u8; 32]) {
    let repair = Box::pin(new.ledger_repair(record)).await.unwrap();
    Box::pin(new.confirm_ledger_repair(record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
}

/// The `ledger_handoff` the record holds, as `app` holds it.
async fn named(app: &Paykit, record: &[u8; 32]) -> Option<LedgerHandoff> {
    held(app, record).await.0.ledger_handoff
}

#[tokio::test]
async fn test_handoff_reader_executor_pays_nothing_until_the_handoff_is_named() {
    // A new executor that kept a version as a reader before its grant builds on none it
    // holds until the previous executor names its last version: that version may predate
    // the previous executor's last. Once named, it pays, the handoff settled.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off_to_a_reader(&w)).await;
    let second = request(&w, &mut shop, &mut new, "0.2").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "confirm a repair"), "{refused:?}");
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let paid = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(new.app().protocol_scan(b"lb/").await.unwrap().is_empty());
    assert!(new.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_handoff_own_version_from_an_earlier_term_pays_nothing_until_named() {
    // History N, P, N: the wallet executes and hands off to wallet2, which pays after its
    // user confirms the repair; the wallet, its consent still held as the notice hasn't
    // reached it, takes the Allowance back. Its version from its first term predates
    // wallet2's writes: it pays nothing until wallet2 names its last version, then counts
    // wallet2's payment.
    let w = World::new();
    let (mut shop, mut n, record) = Box::pin(set_up(
        &w,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    ))
    .await;
    let first = request(&w, &mut shop, &mut n, "0.5").await;
    Box::pin(n.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    // The wallet's later requests reach it now: it reads nothing more, so the notice of the
    // handoff doesn't reach it.
    let third = request(&w, &mut shop, &mut n, "0.3").await;
    let fourth = request(&w, &mut shop, &mut n, "0.2").await;
    let mut p = Box::pin(executor_wallet(&w, "wallet2")).await;
    p.receive(None).await.unwrap();
    Box::pin(p.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    repaired(&mut p, &record).await;
    let second = request(&w, &mut shop, &mut p, "0.3").await;
    let paid = Box::pin(p.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    Box::pin(n.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(held(&n, &record).await.0.grants.len(), 3);
    assert_eq!(n.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
    let refused = Box::pin(n.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "confirm a repair"), "{refused:?}");
    p.receive(None).await.unwrap();
    Box::pin(p.resume_payments()).await.unwrap();
    // Lifetime 1: 0.5 and 0.3 are spent, so 0.3 has no room and 0.2 does.
    let refused = Box::pin(n.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    let paid = Box::pin(n.pay_under_allowance(&fourth, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_handoff_named_later_than_the_repairs_base_is_a_rollback() {
    // The new executor's user confirms the repair before the handoff is named: its first
    // ledger write since the handoff. The previous executor then names a later version than
    // the one the repair built on: it built on an older ledger than the previous executor's
    // last, so the named write raises the alarm.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off_to_a_reader(&w)).await;
    repaired(&mut new, &record).await;
    name_another_version(&mut old, &record, 5, true).await;
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let second = request(&w, &mut shop, &mut new, "0.2").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(is_rollback(&refused), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_handoff_named_before_the_repairs_base_is_ignored_and_told() {
    // The previous executor names a version before the one the repair built on: the new
    // executor ignores it and goes on with its own, and the host is told, with the writer,
    // never silently.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off_to_a_reader(&w)).await;
    repaired(&mut new, &record).await;
    name_another_version(&mut old, &record, -1, true).await;
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let second = request(&w, &mut shop, &mut new, "0.2").await;
    let paid = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(new.ledger_alarm().is_none());
    let notices = new.notices().await.unwrap();
    let told = notices
        .iter()
        .find(|n| n.kind == paykit_sdk::NoticeKind::LedgerHandoffIgnored)
        .unwrap_or_else(|| panic!("{notices:?}"));
    assert_eq!(told.allowance, Some(record));
    assert_eq!(told.writer_app_id, Some(old.app_id()));
    new.acknowledge_notice(&told.id).await.unwrap();
    assert!(new.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_handoff_repair_records_the_item_it_built_on_with_its_batch() {
    // The repair's batch, its answer lost, lands later: what it built on, the write shown,
    // by version and item hash, goes in with it. The previous executor then names another
    // write at that version: the repair built on another write than the previous
    // executor's last, a rollback.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let repair = Box::pin(new.ledger_repair(&record)).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    Box::pin(new.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    new.app_mut().flush_outbox().await.unwrap();
    let mark: Value = serde_json::from_slice(
        &new.app()
            .protocol_get(format!("lb/{}", pubky_switch_core::enc::b64(&record)).as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(
        mark["d"]["handoff"]["first"]["item_hash"].is_array(),
        "{mark}"
    );
    name_another_version(&mut old, &record, 0, true).await;
    Box::pin(old.resume_payments()).await.unwrap();
    new.receive(None).await.unwrap();
    let second = request(&w, &mut shop, &mut new, "0.2").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(is_rollback(&refused), "{refused:?}");
}

#[tokio::test]
async fn test_handoff_previous_executor_with_a_ledger_write_kept_unsent_names_nothing() {
    // The previous executor's last ledger write is kept unsent when the handoff comes: it
    // names nothing while that write has no answer, and names its last version once it has.
    let w = World::new();
    let (mut shop, mut old, record) = Box::pin(set_up(&w, w_terms())).await;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    Box::pin(old.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    let second = request(&w, &mut shop, &mut old, "0.2").await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let _ = Box::pin(old.pay_under_allowance(&second, &record)).await;
    let mut new = Box::pin(executor_wallet(&w, "wallet2")).await;
    new.receive(None).await.unwrap();
    Box::pin(new.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    // A read sends the kept batch again first: it can't reach the switch either time.
    let unreachable = || pubky_switch_client::testing::Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    };
    w.alice.switch().fail_next(unreachable());
    let _ = old.receive(None).await;
    w.alice.switch().fail_next(unreachable());
    Box::pin(old.resume_payments()).await.unwrap();
    assert!(named(&old, &record).await.is_none());
    Box::pin(old.resume_payments()).await.unwrap();
    old.receive(None).await.unwrap();
    assert!(named(&old, &record).await.is_some());
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_handoff_previous_executor_names_nothing_on_a_ledger_read_not_fresh() {
    // The ledger read at the handoff isn't of the switch now: the previous executor names
    // nothing, and names its last version at the next resume.
    let w = World::new();
    let (_shop, mut old, _new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let (allowance, _) = held(&old, &record).await;
    let key = old
        .app()
        .record_key(&crate::ledger::ledger_target(&allowance))
        .unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: format!("records/{}", pubky_switch_core::enc::b64(&key)),
            code: pubky_switch_core::http::ErrorCode::NotFound,
            current: Value::Null,
        });
    Box::pin(old.resume_payments()).await.unwrap();
    assert!(named(&old, &record).await.is_none());
    Box::pin(old.resume_payments()).await.unwrap();
    assert!(named(&old, &record).await.is_some());
}

#[tokio::test]
async fn test_handoff_previous_executor_names_its_own_later_write_over_a_lagging_version() {
    // The previous executor's kept version lags its last write: it names the later of the
    // two, its own write the switch shows.
    let w = World::new();
    let (mut shop, mut old, record) = Box::pin(set_up(&w, w_terms())).await;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    Box::pin(old.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    let lagging = old
        .app()
        .protocol_get(kept_key(&record).as_bytes())
        .await
        .unwrap()
        .unwrap();
    let second = request(&w, &mut shop, &mut old, "0.2").await;
    Box::pin(old.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    let last: Value = serde_json::from_slice(
        &old.app()
            .protocol_get(kept_key(&record).as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    old.app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: kept_key(&record).into_bytes(),
            value: lagging,
        }])
        .await
        .unwrap();
    let mut new = Box::pin(executor_wallet(&w, "wallet2")).await;
    new.receive(None).await.unwrap();
    Box::pin(new.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    old.receive(None).await.unwrap();
    Box::pin(old.resume_payments()).await.unwrap();
    let handoff = named(&old, &record).await.expect("named");
    assert_eq!(
        handoff.ledger.version,
        last["d"]["version"].as_u64().unwrap()
    );
}

#[tokio::test]
async fn test_handoff_named_write_counting_less_than_the_version_kept_pays_nothing() {
    // The new executor kept a version as a reader; the previous executor's later write,
    // which it names, drops a paid entry. Every ledger rule runs against the version kept,
    // also when the named write is taken: it counts less, and nothing is paid on it.
    let w = World::new();
    let (mut shop, mut old, record) = Box::pin(set_up(
        &w,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    ))
    .await;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    Box::pin(old.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    let mut new = Box::pin(executor_wallet(&w, "wallet2")).await;
    new.receive(None).await.unwrap();
    let (allowance, ledger) = held(&old, &record).await;
    let mut less = ledger.unwrap();
    less.entries.clear();
    let target = crate::ledger::ledger_target(&allowance);
    old.app_mut().read_record(&target).await.unwrap();
    let version = old
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .version;
    old.app_mut()
        .put_record(
            &target,
            &less.to_json().unwrap(),
            vec![pubky_switch_core::ops::Condition::RecordVersion {
                key: record,
                version,
            }],
        )
        .await
        .unwrap();
    Box::pin(new.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    old.receive(None).await.unwrap();
    Box::pin(old.resume_payments()).await.unwrap();
    assert!(named(&old, &record).await.is_some());
    new.receive(None).await.unwrap();
    let second = request(&w, &mut shop, &mut new, "0.6").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "counts less than the version kept"),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_handoff_restored_previous_executor_names_nothing() {
    // The previous executor is restored from a backup before the handoff: its version is one
    // a restore installed, which may predate its own later writes, so it names nothing.
    let w = World::new();
    let (mut shop, old, record) = Box::pin(set_up(&w, w_terms())).await;
    let mut old = old;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    Box::pin(old.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    let mut old = Box::pin(restored_confirmed(&w, old, &record)).await;
    let mut new = Box::pin(executor_wallet(&w, "wallet2")).await;
    new.receive(None).await.unwrap();
    Box::pin(new.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    old.receive(None).await.unwrap();
    Box::pin(old.resume_payments()).await.unwrap();
    assert!(named(&old, &record).await.is_none());
}

#[tokio::test]
async fn test_handoff_named_by_a_removed_app_counts_for_nothing() {
    // The previous executor names its last version and is then removed: its handoff counts
    // for nothing, and the new executor pays only through the repair.
    let w = World::new();
    let (mut shop, mut old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    Box::pin(old.resume_payments()).await.unwrap();
    let old_id = old.app_id();
    w.clock.advance_ms(1_000);
    let before = w.clock.now_ms();
    w.alice
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != old_id);
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: old_id,
                distrust_from: before,
            });
        })
        .unwrap();
    w.clock.advance_ms(1_000);
    new.receive(None).await.unwrap();
    assert!(new.app().own_identity().lists_removed(&old_id));
    let second = request(&w, &mut shop, &mut new, "0.2").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "confirm a repair"), "{refused:?}");
}

#[tokio::test]
async fn test_handoff_grant_signed_again_by_the_same_app_waits_for_nothing() {
    // A grant the same app signs again is no handoff: no mark goes into its journal, and it
    // goes on paying.
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, w_terms())).await;
    Box::pin(wallet.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(held(&wallet, &record).await.0.grants.len(), 2);
    assert!(wallet.app().protocol_scan(b"lb/").await.unwrap().is_empty());
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = Box::pin(wallet.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_handoff_previous_executor_that_only_read_its_version_names_nothing() {
    // wallet2 kept its version as a reader, became the executor, and hands the Allowance on
    // to wallet3 before it built on any: it names nothing, and wallet3 pays only through the
    // repair.
    let w = World::new();
    let (mut shop, mut old, mut p, record) = Box::pin(handed_off_to_a_reader(&w)).await;
    Box::pin(old.resume_payments()).await.unwrap();
    let mut q = Box::pin(executor_wallet(&w, "wallet3")).await;
    q.receive(None).await.unwrap();
    Box::pin(q.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    p.receive(None).await.unwrap();
    Box::pin(p.resume_payments()).await.unwrap();
    q.receive(None).await.unwrap();
    let (allowance, _) = held(&q, &record).await;
    assert!(allowance
        .ledger_handoff
        .is_none_or(|h| h.app_id != p.app_id()));
    let second = request(&w, &mut shop, &mut q, "0.2").await;
    let refused = Box::pin(q.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "confirm a repair"), "{refused:?}");
}

#[tokio::test]
async fn test_handoff_second_handoff_before_the_mark_goes_through_the_repair() {
    // N -> P -> Q before P names anything: P holds no version it took, so it names nothing,
    // and Q pays only once its user confirms the repair, counting N's payment.
    let w = World::new();
    let (mut shop, _n, mut p, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let mut q = Box::pin(executor_wallet(&w, "wallet3")).await;
    q.receive(None).await.unwrap();
    Box::pin(q.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    p.receive(None).await.unwrap();
    Box::pin(p.resume_payments()).await.unwrap();
    q.receive(None).await.unwrap();
    let second = request(&w, &mut shop, &mut q, "0.3").await;
    let refused = Box::pin(q.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "confirm a repair"), "{refused:?}");
    let repair = Box::pin(q.ledger_repair(&record)).await.unwrap();
    assert_eq!(repair.lifetime, d("0.5"), "{repair:?}");
    Box::pin(q.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(q.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_handoff_that_doesnt_verify_in_a_write_taken_is_told() {
    // A `ledger_handoff` that doesn't verify reaches the new executor in a write its client
    // took without a copy seen to check it against, its own here: it counts for nothing, the
    // executor waits, and the host is told, with the writer of that write.
    let w = World::new();
    let (mut shop, _old, mut new, record) = Box::pin(handed_off(&w, &["0.5"])).await;
    let (allowance, _) = held(&new, &record).await;
    let keys = w.alice.identity().app_keys("wallet2");
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
    new.app_mut()
        .put_record(&target, &next.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    let second = request(&w, &mut shop, &mut new, "0.2").await;
    let refused = Box::pin(new.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "confirm a repair"), "{refused:?}");
    let notices = new.notices().await.unwrap();
    let told = notices
        .iter()
        .find(|n| n.kind == paykit_sdk::NoticeKind::LedgerHandoffIgnored)
        .unwrap_or_else(|| panic!("{notices:?}"));
    assert_eq!(told.writer_app_id, Some(new.app_id()));
}

fn w_terms() -> paykit_lib::allowances::AllowanceTerms {
    terms_with(json!({}))
}
