//! The Allowance ledger repair (Paykit spec 8.3, "After a confirmation"): a ledger the
//! executor didn't accept is written again, once its user confirms the figures shown, as a
//! merge that never counts less.

use paykit_lib::allowances::Ledger;
use paykit_lib::Decimal;
use paykit_sdk::{LedgerRepair, PayOptions, PayOutcome, Paykit, PaykitError, Refusal};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::Condition;
use serde_json::{json, Value};

use crate::common::World;
use crate::executor::{
    accepted, allowance_shop, executor_wallet, held, ledger_key, refused_for, request, set_up,
    terms_with,
};
use crate::ledger::{is_rollback, ledger_target};

/// `amount` as a decimal.
fn d(amount: &str) -> Decimal {
    Decimal::parse(amount).unwrap()
}

/// The ledger the switch holds for the Allowance at `record`, read by `wallet`.
async fn switch_ledger(wallet: &mut Paykit, record: &[u8; 32]) -> Ledger {
    let (allowance, _) = held(wallet, record).await;
    let target = ledger_target(&allowance);
    wallet.app_mut().read_record(&target).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let shared = wallet.app().shared_record(&key).await.unwrap().unwrap();
    Ledger::from_json(shared.content.value().unwrap()).unwrap()
}

/// Another own app, never the executor, writes `next` as the ledger of the Allowance at
/// `record`, built on the version it reads and conditioned on the Allowance record, as an
/// executor's write is: the check that refuses it is its writer's.
async fn other_writes(other: &mut Paykit, record: &[u8; 32], next: &Ledger) {
    other.receive(None).await.unwrap();
    let (allowance, _) = held(other, record).await;
    let target = ledger_target(&allowance);
    other.app_mut().read_record(&target).await.unwrap();
    let version = other
        .app()
        .shared_record(record)
        .await
        .unwrap()
        .unwrap()
        .version;
    other
        .app_mut()
        .put_record(
            &target,
            &next.to_json().unwrap(),
            vec![Condition::RecordVersion {
                key: *record,
                version,
            }],
        )
        .await
        .unwrap();
}

/// The wallet pays 0.5 under an Allowance with a lifetime limit of 1, then another own app
/// writes the ledger without that entry but with compacted totals of 0.2: a ledger refused
/// by policy, its writer not the executor. Returns the shop, the wallet, the other app and
/// the Allowance record's key.
async fn refused_by_policy(w: &World) -> (Paykit, Paykit, Paykit, [u8; 32]) {
    let (mut shop, mut wallet, record) = Box::pin(set_up(
        w,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    ))
    .await;
    let first = request(w, &mut shop, &mut wallet, "0.5").await;
    Box::pin(wallet.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    let mut other = Box::pin(executor_wallet(w, "other")).await;
    let kept = switch_ledger(&mut wallet, &record).await;
    let rewritten = Ledger {
        entries: Vec::new(),
        spent: Some(d("0.2")),
        count: 1,
        ..kept
    };
    other_writes(&mut other, &record, &rewritten).await;
    (shop, wallet, other, record)
}

#[tokio::test]
async fn test_repair_ledger_refused_by_policy_resumes_payments() {
    // The executor refuses the other app's ledger, and pays nothing. The repair shows the
    // merge of the version it kept, with the paid entry, and what the ledger shown adds, its
    // totals: 0.7 of the lifetime limit. Confirmed, that ledger is written and payments
    // resume within what is left, never counting less than either.
    let w = World::new();
    let (mut shop, mut wallet, _other, record) = Box::pin(refused_by_policy(&w)).await;
    let second = request(&w, &mut shop, &mut wallet, "0.3").await;
    let refused = Box::pin(wallet.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "the ledger's writer isn't the executor"),
        "{refused:?}"
    );
    let repair = Box::pin(wallet.ledger_repair(&record)).await.unwrap();
    assert!(
        repair.reason.contains("writer isn't the executor"),
        "{repair:?}"
    );
    assert_eq!(repair.lifetime, d("0.7"), "{repair:?}");
    assert_eq!((repair.reserved, repair.spent, repair.released), (0, 1, 0));
    assert!(repair.periods.is_empty());
    assert!(repair.reserved_by_other_apps.is_empty());
    assert_eq!(w.rail.all_executions().len(), 1);
    Box::pin(wallet.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    let written = switch_ledger(&mut wallet, &record).await;
    assert_eq!(written.entries.len(), 1);
    assert_eq!(written.spent, Some(d("0.2")));
    let paid = Box::pin(wallet.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let third = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = Box::pin(wallet.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_repair_ledger_missing_after_its_first_write_got_no_answer() {
    // The first grant's empty ledger is owed and refused, so the first payment's batch
    // carries the ledger's first write, marked sent; that batch gets no answer. The switch
    // then shows no ledger: usage history lost. With no version kept, the repair starts from
    // the empty ledger, the journal's attempt and its claim: 0.4 reserved of the lifetime
    // limit of 1, a payment made by hand left out. Confirmed, the executor pays again within
    // what is left.
    let w = World::new();
    let mut shop = Box::pin(allowance_shop(&w)).await;
    let mut wallet = Box::pin(executor_wallet(&w, "wallet")).await;
    let record = Box::pin(accepted(
        &w,
        &mut shop,
        &mut wallet,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    ))
    .await;
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(wallet.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    let first = request(&w, &mut shop, &mut wallet, "0.4").await;
    w.alice.switch().fail_next(Fault::Error {
        path: "/ops".into(),
        code: ErrorCode::PermissionMissing,
        current: Value::Null,
    });
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let _ = Box::pin(wallet.pay_under_allowance(&first, &record)).await;
    assert!(w.rail.all_executions().is_empty());
    // The kept batch lands with the next read; the switch then hides the ledger.
    wallet.receive(None).await.unwrap();
    let key = ledger_key(&wallet, &record).await;
    assert_eq!(w.record_writes(&w.alice, &key), 1);
    assert!(wallet.app().protocol_scan(b"lv/").await.unwrap().is_empty());
    // The user pays another of bob's requests by hand: its claim shows the executor's
    // attempt, and its proof names no Allowance, so the repair leaves it out.
    let by_hand = request(&w, &mut shop, &mut wallet, "0.2").await;
    let paid = Box::pin(wallet.pay_request(&by_hand, PayOptions::default()))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let hide = || Fault::Error {
        path: format!("records/{}", pubky_switch_core::enc::b64(&key)),
        code: ErrorCode::NotFound,
        current: Value::Null,
    };
    w.alice.switch().fail_next(hide());
    let second = request(&w, &mut shop, &mut wallet, "0.6").await;
    let refused = Box::pin(wallet.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "usage history lost"), "{refused:?}");
    w.alice.switch().fail_next(hide());
    let repair = Box::pin(wallet.ledger_repair(&record)).await.unwrap();
    assert!(repair.reason.contains("usage history lost"), "{repair:?}");
    assert_eq!(repair.lifetime, d("0.4"), "{repair:?}");
    assert_eq!((repair.reserved, repair.spent, repair.released), (1, 0, 0));
    w.alice.switch().fail_next(hide());
    Box::pin(wallet.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), 2);
    assert_eq!(wallet.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
    let paid = Box::pin(wallet.pay_under_allowance(&second, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let third = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = Box::pin(wallet.pay_under_allowance(&third, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
}

/// The repair `error` carries: refused because what it was built from changed.
fn changed(error: &PaykitError) -> Option<&LedgerRepair> {
    match error.refusal() {
        Some(Refusal::LedgerRepairChanged(repair)) => Some(repair),
        _ => None,
    }
}

#[tokio::test]
async fn test_repair_figures_changed_before_the_confirmation_are_refused() {
    // Between the figures shown and the user's confirmation, the other app writes the
    // ledger again, with higher totals. The confirmation writes nothing and carries the
    // figures computed now; confirming those writes the ledger.
    let w = World::new();
    let (_shop, mut wallet, mut other, record) = Box::pin(refused_by_policy(&w)).await;
    let repair = Box::pin(wallet.ledger_repair(&record)).await.unwrap();
    assert_eq!(repair.lifetime, d("0.7"));
    let shown = switch_ledger(&mut other, &record).await;
    other_writes(
        &mut other,
        &record,
        &Ledger {
            spent: Some(d("0.3")),
            count: 2,
            ..shown
        },
    )
    .await;
    let key = ledger_key(&wallet, &record).await;
    let writes = w.record_writes(&w.alice, &key);
    let refused = Box::pin(wallet.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap_err();
    let now = changed(&refused).unwrap_or_else(|| panic!("{refused:?}"));
    assert_eq!(now.lifetime, d("0.8"), "{now:?}");
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    let now = now.clone();
    Box::pin(wallet.confirm_ledger_repair(&record, &now, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), writes + 1);
    assert_eq!(
        switch_ledger(&mut wallet, &record).await.spent,
        Some(d("0.3"))
    );
}

#[tokio::test]
async fn test_repair_refused_while_the_rollback_alarm_stands() {
    // The executor finds a rollback of one Allowance's ledger: the alarm holds every
    // attempt at that generation. A second Allowance, whose ledger another own app wrote,
    // isn't repaired while it stands, though nothing else of it would refuse.
    let w = World::new();
    let (mut shop, mut wallet, rolled) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let second = Box::pin(accepted(&w, &mut shop, &mut wallet, terms_with(json!({})))).await;
    Box::pin(wallet.grant_executor(&second, UserConfirmed::new()))
        .await
        .unwrap();
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    let empty = switch_ledger(&mut wallet, &second).await;
    other_writes(&mut other, &second, &empty).await;
    // The version kept of the first is said to be later than any the switch shows.
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    Box::pin(wallet.pay_under_allowance(&first, &rolled))
        .await
        .unwrap();
    let kept = format!("lv/{}", pubky_switch_core::enc::b64(&rolled));
    let held_kept = wallet
        .app()
        .protocol_get(kept.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held_kept).unwrap();
    value["d"]["version"] = json!(value["d"]["version"].as_u64().unwrap() + 5);
    value["d"]["item_hash"] = json!(pubky_switch_core::enc::b64(&[7; 32]));
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: kept.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    let next = request(&w, &mut shop, &mut wallet, "0.5").await;
    let alarm = Box::pin(wallet.pay_under_allowance(&next, &rolled))
        .await
        .unwrap_err();
    assert!(is_rollback(&alarm), "{alarm:?}");
    let refused = Box::pin(wallet.ledger_repair(&second)).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(why)) if why.contains("rollback alarm")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_repair_refused_to_an_app_that_isnt_the_executor() {
    // The other app holds `paykit_allowance_execute` but isn't the Allowance's executor:
    // only the current executor repairs its ledger.
    let w = World::new();
    let (_shop, mut wallet, mut other, record) = Box::pin(refused_by_policy(&w)).await;
    let refused = Box::pin(other.ledger_repair(&record)).await.unwrap_err();
    assert!(
        refused_for(&refused, "another app is the Allowance's executor"),
        "{refused:?}"
    );
    // The executor's own repair, shown, isn't the other app's to confirm either.
    let repair = Box::pin(wallet.ledger_repair(&record)).await.unwrap();
    let key = ledger_key(&wallet, &record).await;
    let writes = w.record_writes(&w.alice, &key);
    let refused = Box::pin(other.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "another app is the Allowance's executor"),
        "{refused:?}"
    );
    assert_eq!(w.record_writes(&w.alice, &key), writes);
}

#[tokio::test]
async fn test_repair_refused_when_a_claim_read_fails() {
    // A claim the repair reads fails: no figures come from a partial read, and a
    // confirmation then writes nothing.
    let w = World::new();
    let (_shop, mut wallet, _other, record) = Box::pin(refused_by_policy(&w)).await;
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/claims/".into(),
        after: 0,
    });
    assert!(Box::pin(wallet.ledger_repair(&record)).await.is_err());
    let repair = Box::pin(wallet.ledger_repair(&record)).await.unwrap();
    let key = ledger_key(&wallet, &record).await;
    let writes = w.record_writes(&w.alice, &key);
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/claims/".into(),
        after: 0,
    });
    assert!(
        Box::pin(wallet.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
            .await
            .is_err()
    );
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    Box::pin(wallet.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), writes + 1);
}

#[tokio::test]
async fn test_repair_ledger_at_its_key_before_the_first_grant_lands_after_no_answer() {
    // Another own app wrote a ledger at the Allowance's key before the first grant, with
    // compacted totals of 0.5: the executor takes it as one it didn't accept. The repair
    // takes those totals; its write gets no answer, and lands with the next read. The mark
    // of the ledger it didn't accept goes with the write, so the executor then pays on its
    // own write, counting the 0.5.
    let w = World::new();
    let mut shop = Box::pin(allowance_shop(&w)).await;
    let mut wallet = Box::pin(executor_wallet(&w, "wallet")).await;
    let record = Box::pin(accepted(
        &w,
        &mut shop,
        &mut wallet,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    ))
    .await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    let target = ledger_target(&allowance);
    other.app_mut().read_record(&target).await.unwrap();
    let left = Ledger {
        spent: Some(d("0.5")),
        count: 1,
        ..Ledger::empty(allowance.counterparty, allowance.allowance_id.clone())
    };
    other
        .app_mut()
        .put_record(&target, &left.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    Box::pin(wallet.grant_executor(&record, UserConfirmed::new()))
        .await
        .unwrap();
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = Box::pin(wallet.pay_under_allowance(&first, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "at its key before the first grant"),
        "{refused:?}"
    );
    let repair = Box::pin(wallet.ledger_repair(&record)).await.unwrap();
    assert_eq!(repair.lifetime, d("0.5"), "{repair:?}");
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(wallet.confirm_ledger_repair(&record, &repair, UserConfirmed::new()))
        .await
        .unwrap();
    let key = ledger_key(&wallet, &record).await;
    let writes = w.record_writes(&w.alice, &key);
    // The kept batch lands with the next read, and nothing of it is kept yet.
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), writes + 1);
    assert!(wallet.app().protocol_scan(b"lv/").await.unwrap().is_empty());
    let paid = Box::pin(wallet.pay_under_allowance(&first, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // The payment's entry, then its outcome.
    assert_eq!(w.record_writes(&w.alice, &key), writes + 3);
    let second = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = Box::pin(wallet.pay_under_allowance(&second, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
}
