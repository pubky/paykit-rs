//! The Allowance ledger (Paykit spec 8.3, Ledger): outcomes written into its entries, its
//! compaction near the size limit, and the rollback alarm of the last ledger version.

use paykit_lib::allowances::{EntryOutcome, Ledger, LedgerEntry};
use paykit_lib::records::AllowanceRecord;
use paykit_lib::{Asset, AttemptId, Decimal};
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitError, Refusal};
use pubky_switch_client::app::{Alarm, AppError, RecordTarget, UserConfirmed};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::ops::Condition;
use serde_json::{json, Value};

use crate::common::World;
use crate::executor::{held, ledger_key, refused_for, request, set_up, terms_with};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Whether `error` is the alarm of a rollback (Switch spec 10.1).
pub(crate) fn is_rollback(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Client(client)
        if matches!(client.unkept(), AppError::Alarm(Alarm::Rollback { .. })))
}

fn ledger_target(allowance: &AllowanceRecord) -> RecordTarget {
    RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    )
}

/// The wallet's ledger, as it holds it.
async fn ledger(wallet: &Paykit, record: &[u8; 32]) -> Ledger {
    held(wallet, record).await.1.expect("a ledger is held")
}

/// The ledger the switch holds now, read by the wallet.
async fn fresh_ledger(wallet: &mut Paykit, record: &[u8; 32]) -> Ledger {
    let (allowance, _) = held(wallet, record).await;
    wallet
        .app_mut()
        .read_record(&ledger_target(&allowance))
        .await
        .unwrap();
    ledger(wallet, record).await
}

/// An entry of attempt `n` for `value` btc at `at`, with `outcome`.
fn entry(n: u32, value: &str, at: u64, outcome: Option<EntryOutcome>) -> LedgerEntry {
    let mut obligation = [0xa5; 32];
    obligation[..4].copy_from_slice(&n.to_be_bytes());
    let mut attempt = [0x5a; 16];
    attempt[..4].copy_from_slice(&n.to_be_bytes());
    LedgerEntry {
        obligation,
        attempt_id: AttemptId::from_bytes(attempt),
        value: Decimal::parse(value).unwrap(),
        asset: Asset::parse("btc").unwrap(),
        at,
        outcome,
    }
}

/// The executor writes `next` as its ledger itself, built on the version it holds and
/// conditioned on the Allowance record, as its own batches are.
async fn write_ledger(wallet: &mut Paykit, record: &[u8; 32], next: &Ledger) {
    let (allowance, _) = held(wallet, record).await;
    let target = ledger_target(&allowance);
    wallet.app_mut().read_record(&target).await.unwrap();
    let version = wallet
        .app()
        .shared_record(record)
        .await
        .unwrap()
        .unwrap()
        .version;
    wallet
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

/// The ledger with entries of `value` btc each, dated `at` with `outcome`, added until it
/// is `len` bytes long or more.
fn filled(
    base: &Ledger,
    value: &str,
    at: u64,
    outcome: Option<EntryOutcome>,
    len: usize,
) -> Ledger {
    let mut next = base.clone();
    let mut n = 0;
    while next.to_json().unwrap().len() < len {
        // A hundred at a time: the value is written whole for each check.
        for _ in 0..100 {
            next.entries.push(entry(n, value, at, outcome));
            n += 1;
        }
    }
    next
}

#[tokio::test]
async fn test_ledger_outcome_done_goes_with_the_payment() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let ledger = ledger(&wallet, &record).await;
    assert_eq!(ledger.entries.len(), 1);
    assert_eq!(ledger.entries[0].outcome, Some(EntryOutcome::Done));
    // The version the outcome's batch wrote is the one kept.
    let key = format!("lv/{}", pubky_switch_core::enc::b64(&record));
    let kept: Value = serde_json::from_slice(
        &wallet
            .app()
            .protocol_get(key.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let value = pubky_switch_core::enc::unb64(kept["d"]["value"].as_str().unwrap()).unwrap();
    assert_eq!(Ledger::from_json(&value).unwrap(), ledger);
}

#[tokio::test]
async fn test_ledger_outcome_failed_by_the_owner_goes_into_its_entry() {
    // The executor batch is applied and its answer lost; the resume writes the attempt
    // `failed` on its claim and in its entry, as its owner, in one batch, so the entry
    // releases its amount by its own outcome whatever becomes of the claim: two more of 1
    // fit the lifetime limit of 2.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(wallet.pay_under_allowance(&first, &record).await.is_err());
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    let failed = ledger(&wallet, &record).await;
    assert_eq!(failed.entries.len(), 1);
    assert_eq!(failed.entries[0].outcome, Some(EntryOutcome::Failed));
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&second, &record).await.unwrap();
    let third = request(&w, &mut shop, &mut wallet, "1").await;
    let paid = wallet.pay_under_allowance(&third, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_ledger_outcome_written_twice_counts_once() {
    // The outcome's batch is applied and its answer lost: the resume writes it again, which
    // finds it written. The entry is `done` once, and the lifetime limit of 2 counts it once.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    w.alice.switch().fail_next(Fault::LoseAnswerAfter {
        path: "/ops".into(),
        after: 1,
    });
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(
        matches!(paid, PayOutcome::PaidOutcomeUnwritten { .. }),
        "{paid:?}"
    );
    wallet.resume_payments().await.unwrap();
    wallet.resume_payments().await.unwrap();
    let ledger = ledger(&wallet, &record).await;
    assert_eq!(ledger.entries.len(), 1);
    assert_eq!(ledger.entries[0].outcome, Some(EntryOutcome::Done));
    assert_eq!(ledger.count, 0);
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&second, &record).await.unwrap();
    let third = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&third, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_ledger_outcome_left_on_the_claim_is_copied_at_resume() {
    // The ledger can't be read when the outcome is written, so `done` goes on the claim
    // alone; the next resume copies it into the entry from the claim.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    let key = ledger_key(&wallet, &record).await;
    w.alice.switch().fail_next(Fault::Unreachable {
        path: format!("records/{}", pubky_switch_core::enc::b64(&key)),
        after: 1,
    });
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(ledger(&wallet, &record).await.entries[0].outcome, None);
    wallet.resume_payments().await.unwrap();
    let copied = ledger(&wallet, &record).await;
    assert_eq!(copied.entries.len(), 1);
    assert_eq!(copied.entries[0].outcome, Some(EntryOutcome::Done));
}

/// An executor attempt of `value` btc whose batch landed and whose answer was lost, then
/// written `failed` on its claim by another own app: reserved until the user releases it.
async fn failed_by_another_app(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    record: &[u8; 32],
    value: &str,
) -> LedgerEntry {
    let first = request(w, shop, wallet, value).await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(wallet.pay_under_allowance(&first, record).await.is_err());
    let entry = fresh_ledger(wallet, record).await.entries[0].clone();
    w.alice
        .switch()
        .fail_attempt(entry.obligation, entry.attempt_id.to_bytes(), [0x5b; 16]);
    entry
}

#[tokio::test]
async fn test_ledger_release_frees_an_attempt_another_app_failed() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let entry = failed_by_another_app(&w, &mut shop, &mut wallet, &record, "1").await;
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&second, &record).await.unwrap();
    // The other app's `failed` frees nothing: 1 reserved and 1 spent fill the limit of 2.
    let third = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&third, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    let reserved = wallet.reserved_by_other_apps(&record).await.unwrap();
    assert_eq!(reserved.len(), 1);
    assert_eq!(reserved[0].attempt_id, entry.attempt_id);
    assert_eq!(reserved[0].value.as_str(), "1");
    wallet
        .release_allowance_attempt(&record, entry.attempt_id, UserConfirmed::new())
        .await
        .unwrap();
    let released = ledger(&wallet, &record).await;
    let held = released
        .entries
        .iter()
        .find(|e| e.attempt_id == entry.attempt_id)
        .unwrap();
    assert_eq!(held.outcome, Some(EntryOutcome::Failed));
    assert!(wallet
        .reserved_by_other_apps(&record)
        .await
        .unwrap()
        .is_empty());
    let paid = wallet.pay_under_allowance(&third, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_ledger_release_refused_for_an_attempt_still_open() {
    // An attempt whose claim shows it open, not failed by another app, isn't the user's to
    // release: it may still be paid.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(wallet.pay_under_allowance(&first, &record).await.is_err());
    let entry = fresh_ledger(&mut wallet, &record).await.entries[0].clone();
    assert!(wallet
        .reserved_by_other_apps(&record)
        .await
        .unwrap()
        .is_empty());
    let refused = wallet
        .release_allowance_attempt(&record, entry.attempt_id, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
    assert_eq!(ledger(&wallet, &record).await.entries[0].outcome, None);
}

/// A ledger of the executor's own, its entries old and paid, past the bound of about a
/// thousand payments a record held before compaction: 0.0001 each, so more than 0.1 in all,
/// under a lifetime limit of 1.
async fn past_the_old_bound(w: &World) -> (Paykit, Paykit, [u8; 32], Ledger) {
    let (shop, mut wallet, record) =
        set_up(w, terms_with(json!({ "lifetime_amount_limit": "1" }))).await;
    let base = ledger(&wallet, &record).await;
    let old = w.clock.now_ms() - 10 * DAY_MS;
    let full = filled(&base, "0.0001", old, Some(EntryOutcome::Done), 230_000);
    assert!(full.entries.len() > 1_000);
    write_ledger(&mut wallet, &record, &full).await;
    (shop, wallet, record, full)
}

/// The paid amount `full`'s entries hold.
fn paid_in(full: &Ledger) -> Decimal {
    let scaled: u128 = full.entries.iter().map(|e| e.value.scaled()).sum();
    Decimal::from_scaled(scaled)
}

/// After compaction the totals hold what the entries held: paying up to the lifetime
/// limit of 1 leaves no room for 0.01 more.
async fn counts_the_compacted(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    record: &[u8; 32],
    full: &Ledger,
) {
    let compacted = ledger(wallet, record).await;
    assert!(compacted.entries.len() < 5, "{}", compacted.entries.len());
    assert_eq!(compacted.count, u64::try_from(full.entries.len()).unwrap());
    assert_eq!(compacted.spent, Some(paid_in(full)));
    let paid = paid_in(full).scaled()
        + compacted
            .entries
            .iter()
            .filter(|e| e.outcome == Some(EntryOutcome::Done))
            .map(|e| e.value.scaled())
            .sum::<u128>();
    let room = Decimal::from_scaled(Decimal::parse("1").unwrap().scaled() - paid);
    let last = request(w, shop, wallet, &room.normalized()).await;
    let made = wallet.pay_under_allowance(&last, record).await.unwrap();
    assert!(matches!(made, PayOutcome::Paid { .. }), "{made:?}");
    let over = request(w, shop, wallet, "0.01").await;
    let refused = wallet.pay_under_allowance(&over, record).await.unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
}

#[tokio::test]
async fn test_ledger_compaction_keeps_paying_past_the_old_bound() {
    let w = World::new();
    let (mut shop, mut wallet, record, full) = past_the_old_bound(&w).await;
    let next = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet.pay_under_allowance(&next, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
    counts_the_compacted(&w, &mut shop, &mut wallet, &record, &full).await;
}

#[tokio::test]
async fn test_ledger_compaction_crash_after_the_switch_applied_it_counts_the_same() {
    // The compacting batch is applied and its answer lost, so no version is kept from it:
    // the next write builds on it, judged against the version kept before, and the totals
    // count what the entries did.
    let w = World::new();
    let (mut shop, mut wallet, record, full) = past_the_old_bound(&w).await;
    let next = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(wallet.pay_under_allowance(&next, &record).await.is_err());
    wallet.resume_payments().await.unwrap();
    assert!(w.rail.all_executions().is_empty());
    counts_the_compacted(&w, &mut shop, &mut wallet, &record, &full).await;
}

#[tokio::test]
async fn test_ledger_compaction_crash_before_the_switch_applied_it_counts_the_same() {
    // The compacting batch never reaches the switch: kept, it goes at the resume, or the
    // next payment compacts again; either way the totals count what the entries did.
    let w = World::new();
    let (mut shop, mut wallet, record, full) = past_the_old_bound(&w).await;
    let next = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    assert!(wallet.pay_under_allowance(&next, &record).await.is_err());
    wallet.resume_payments().await.unwrap();
    counts_the_compacted(&w, &mut shop, &mut wallet, &record, &full).await;
}

#[tokio::test]
async fn test_ledger_counting_less_is_ignored_and_pays_nothing() {
    // The executor's own app writes a version built on the one kept that drops a paid entry:
    // it counts less, so it is refused as untrusted and ignored, the Allowance paused
    // without an alarm, and nothing is paid on it.
    let w = World::new();
    let (mut shop, mut wallet, record) =
        set_up(&w, terms_with(json!({ "lifetime_amount_limit": "1" }))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&first, &record).await.unwrap();
    let kept = wallet.app().protocol_scan(b"lv/").await.unwrap();
    let mut less = ledger(&wallet, &record).await;
    less.entries.clear();
    write_ledger(&mut wallet, &record, &less).await;
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    let refused = wallet
        .pay_under_allowance(&second, &record)
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "counts less than the version kept"),
        "{refused:?}"
    );
    assert!(wallet.ledger_alarm().is_none());
    assert_eq!(wallet.app().protocol_scan(b"lv/").await.unwrap(), kept);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_ledger_shown_neither_kept_nor_built_on_raises_the_alarm() {
    // The version kept is later than any the ledger shown builds on, and isn't the write
    // shown: the service shows an older ledger, a rollback. The alarm stops every new
    // attempt, by hand too.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet.pay_under_allowance(&first, &record).await.unwrap();
    let key = format!("lv/{}", pubky_switch_core::enc::b64(&record));
    let held = wallet
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held).unwrap();
    value["d"]["version"] = json!(value["d"]["version"].as_u64().unwrap() + 5);
    value["d"]["item_hash"] = json!(pubky_switch_core::enc::b64(&[7; 32]));
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    let second = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&second, &record)
        .await
        .unwrap_err();
    assert!(is_rollback(&refused), "{refused:?}");
    assert!(matches!(
        wallet.ledger_alarm(),
        Some(Alarm::Rollback { .. })
    ));
    let by_hand = wallet
        .pay_request(&second, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(by_hand.refusal(), Some(Refusal::NotNow(_))),
        "{by_hand:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_ledger_outcome_done_over_failed_when_the_payment_was_made() {
    // The method reports the dispatch failed, so `failed` goes on the claim and into the
    // entry; its history then shows the payment made. The entry becomes `done`, and the
    // lifetime limit of 2 counts it: one more of 1 fits, and nothing after it.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let PayOutcome::Failed { attempt_id, .. } =
        wallet.pay_under_allowance(&first, &record).await.unwrap()
    else {
        panic!("failed");
    };
    assert_eq!(
        ledger(&wallet, &record).await.entries[0].outcome,
        Some(EntryOutcome::Failed)
    );
    w.rail.complete_after_failure(&attempt_id);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(
        ledger(&wallet, &record).await.entries[0].outcome,
        Some(EntryOutcome::Done)
    );
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&second, &record).await.unwrap();
    let third = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&third, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
}

#[tokio::test]
async fn test_ledger_release_refused_for_an_attempt_its_journal_shows_paid() {
    // The payment is made and its outcome's answer lost; another app then writes the
    // attempt `failed` on its claim. The journal shows it paid, so it isn't the user's to
    // release.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    w.alice.switch().fail_next(Fault::LoseAnswerAfter {
        path: "/ops".into(),
        after: 1,
    });
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(
        matches!(paid, PayOutcome::PaidOutcomeUnwritten { .. }),
        "{paid:?}"
    );
    let entry = fresh_ledger(&mut wallet, &record).await.entries[0].clone();
    w.alice
        .switch()
        .fail_attempt(entry.obligation, entry.attempt_id.to_bytes(), [0x5b; 16]);
    assert!(wallet
        .reserved_by_other_apps(&record)
        .await
        .unwrap()
        .is_empty());
    let refused = wallet
        .release_allowance_attempt(&record, entry.attempt_id, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_ledger_outcome_done_from_a_previous_executors_proof() {
    // The executor's method reports a dispatch failed, so `failed` goes on the claim and
    // into the entry. The user hands off to a new executor; the previous one's history
    // then shows the payment made, and it records its proof in the payer record, the
    // ledger no longer its to write. The new executor writes `done` over the `failed` from
    // the proof naming the Allowance, and the lifetime limit of 2 counts it.
    let w = World::new();
    let (mut shop, mut old, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut old, "1").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let PayOutcome::Failed { attempt_id, .. } =
        old.pay_under_allowance(&first, &record).await.unwrap()
    else {
        panic!("failed");
    };
    let mut new = crate::executor::executor_wallet(&w, "wallet2").await;
    new.receive(None).await.unwrap();
    new.grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    old.receive(None).await.unwrap();
    w.rail.complete_after_failure(&attempt_id);
    let resumed = old.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    // The previous executor wrote no ledger version.
    assert_eq!(
        fresh_ledger(&mut new, &record).await.entries[0].outcome,
        Some(EntryOutcome::Failed)
    );
    new.receive(None).await.unwrap();
    new.resume_payments().await.unwrap();
    let copied = ledger(&new, &record).await;
    assert_eq!(copied.entries[0].attempt_id, attempt_id);
    assert_eq!(copied.entries[0].outcome, Some(EntryOutcome::Done));
    let second = request(&w, &mut shop, &mut new, "1").await;
    new.pay_under_allowance(&second, &record).await.unwrap();
    let third = request(&w, &mut shop, &mut new, "0.1").await;
    let refused = new.pay_under_allowance(&third, &record).await.unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
}
