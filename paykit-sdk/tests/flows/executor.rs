//! The executor (Paykit spec 8.3): the grant, the ledger, and paying a one-time request
//! automatically under an Allowance.

use std::sync::Arc;

use paykit_lib::allowances::{AllowanceTerms, Ledger};
use paykit_lib::records::AllowanceRecord;
use paykit_sdk::{
    AllowanceRole, AllowanceStatus, AppScopes, PayOptions, PayOutcome, Paykit, PaykitError,
    Refusal, RequestRef, RequestStatus,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use pubky_switch_client::transport::Transport;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde_json::{json, Value};

use crate::common::{draft, request_from, ReadHook, World};

fn terms_with(extra: Value) -> AllowanceTerms {
    let mut terms = json!({
        "asset": "btc",
        "per_payment_amount": { "minimum": "0.0001", "maximum": "1" },
        "period_limits": [],
        "lifetime_amount_limit": "2",
        "active_from": null,
        "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    });
    for (k, v) in extra.as_object().unwrap() {
        terms[k] = v.clone();
    }
    AllowanceTerms::from_json(&terms).unwrap()
}

/// Alice's executor wallet: `paykit_pay`, `paykit_allowance`, `paykit_allowance_execute` and
/// `paykit_contacts`; she admits bob.
pub(crate) async fn executor_wallet(w: &World, name: &str) -> Paykit {
    let mut app = w
        .join(
            &w.alice,
            name,
            AppScopes::new()
                .pay()
                .allowance()
                .allowance_execute()
                .contacts()
                .build()
                .unwrap(),
        )
        .await;
    app.app_mut().invite(&w.bob_id()).await.unwrap();
    app
}

/// Bob's shop, holding `paykit_request` and `paykit_allowance`; he admits alice.
pub(crate) async fn allowance_shop(w: &World) -> Paykit {
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

/// Bob proposes an Allowance under `terms` to alice as allowee; the wallet accepts it.
/// Returns the wallet's record key.
pub(crate) async fn accepted(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    terms: AllowanceTerms,
) -> [u8; 32] {
    let sent = shop
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms)
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let record = wallet
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|v| v.allowance_id == sent.allowance_id)
        .unwrap()
        .record;
    wallet.accept_allowance(&record).await.unwrap();
    record
}

/// An Allowance under `terms`, accepted, with the wallet as its executor.
async fn set_up(w: &World, terms: AllowanceTerms) -> (Paykit, Paykit, [u8; 32]) {
    let mut shop = allowance_shop(w).await;
    let mut wallet = executor_wallet(w, "wallet").await;
    let record = accepted(w, &mut shop, &mut wallet, terms).await;
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    (shop, wallet, record)
}

/// Bob's request for `value` btc, received by the wallet.
async fn request(w: &World, shop: &mut Paykit, wallet: &mut Paykit, value: &str) -> RequestRef {
    let request = request_from(shop, w.alice_id(), draft(value, w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    request
}

/// A request of bob's for `value` btc made after the wallet's restore, as its executor may
/// pay it: dated after the block floor the restore owes plus the clock tolerance, and, as
/// every request open while the restore's hold lasts, confirmed by the user as possibly
/// cancelled (Paykit spec 7.2, "What a restore loses"; 8.1, "Blocked counterparties").
async fn request_after_restore(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    value: &str,
) -> RequestRef {
    w.clock.advance_ms(CLOCK_TOLERANCE_MS + 1);
    let request = request(w, shop, wallet, value).await;
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    request
}

fn not_automatic(error: &PaykitError) -> bool {
    matches!(error.refusal(), Some(Refusal::NotAutomatic(_)))
}

/// Whether `error` is the executor's refusal for the check whose rule says `why`.
fn refused_for(error: &PaykitError, why: &str) -> bool {
    matches!(error.refusal(), Some(Refusal::NotAutomatic(rule)) if rule.contains(why))
}

/// The wallet's Allowance record and ledger, as it holds them.
async fn held(wallet: &Paykit, record: &[u8; 32]) -> (AllowanceRecord, Option<Ledger>) {
    let shared = wallet.app().shared_record(record).await.unwrap().unwrap();
    let allowance =
        AllowanceRecord::read(shared.content.value().unwrap(), &wallet.identity()).unwrap();
    let mut ledger = None;
    for held in wallet.app().records_of("allowance_ledgers").await.unwrap() {
        if let Some(value) = held.content.value() {
            ledger = Some(Ledger::from_json(value).unwrap());
        }
    }
    (allowance, ledger)
}

#[tokio::test]
async fn test_executor_grant_writes_the_history_and_the_empty_ledger() {
    let w = World::new();
    let (_shop, wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let (allowance, ledger) = held(&wallet, &record).await;
    assert_eq!(allowance.grants.len(), 1);
    let (grant, _) = allowance.current_grant(&wallet.identity()).unwrap();
    assert_eq!(grant.app_id, wallet.app_id());
    let ledger = ledger.expect("the empty ledger follows the first grant");
    assert!(ledger.entries.is_empty());
    assert!(wallet.allowance(&record).await.unwrap().consented);
}

#[tokio::test]
async fn test_executor_pays_a_request_under_the_allowance() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let (_, ledger) = held(&wallet, &record).await;
    let ledger = ledger.unwrap();
    assert_eq!(ledger.entries.len(), 1);
    assert_eq!(ledger.entries[0].value.to_string(), "0.5");
}

#[tokio::test]
async fn test_executor_refuses_a_payment_over_the_lifetime_limit() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&first, &record).await.unwrap();
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    wallet.pay_under_allowance(&second, &record).await.unwrap();
    let third = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&third, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    // By hand, the user may still pay it.
    wallet
        .pay_request(&third, PayOptions::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_executor_grant_refused_without_paykit_allowance_execute() {
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            AppScopes::new().pay().allowance().build().unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    let refused = wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::ScopeMissing(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_grant_refused_before_acceptance_and_for_the_allowee() {
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    shop.propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms_with(json!({})))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let record = wallet.allowances().await.unwrap()[0].record;
    let refused = wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
    // Alice proposes to bob as allowee: bob pays, so her apps never execute it.
    let sent = wallet
        .propose_allowance(w.bob_id(), AllowanceRole::Allowee, terms_with(json!({})))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let theirs = shop
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|v| v.allowance_id == sent.allowance_id)
        .unwrap()
        .record;
    shop.accept_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .grant_executor(&sent.record, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_grant_refused_once_ended() {
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    wallet.end_allowance(&record).await.unwrap();
    let refused = wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::AllowanceEnded | Refusal::Usage(_))
        ),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_batch_carries_the_allowance_version_and_the_time_window() {
    use pubky_switch_core::ops::{Condition, OpKind};
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let before = w.alice.switch().applied().len();
    wallet.pay_under_allowance(&request, &record).await.unwrap();
    let applied = w.alice.switch().applied();
    let version = wallet
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .version;
    let ledger_write = applied[before..]
        .iter()
        .find(|op| {
            op.body()
                .conditions
                .iter()
                .any(|c| matches!(c, Condition::RecordVersion { key, .. } if key == &record))
        })
        .expect("the ledger's write is conditioned on the Allowance record");
    assert!(matches!(ledger_write.body().kind, OpKind::RecordPut(_)));
    let window = ledger_write.body().conditions.iter().find_map(|c| match c {
        Condition::ServerTimeIn {
            start: Some(at),
            end: Some(end),
        } => Some((*at, *end)),
        _ => None,
    });
    let (at, end) = window.expect("server_time_in(at, end)");
    assert!(end > at && end - at <= 60_000);
    assert!(ledger_write.body().conditions.iter().any(|c| matches!(
        c,
        Condition::RecordVersion { key, version: v } if key == &record && *v == version
    )));
}

#[tokio::test]
async fn test_executor_names_the_allowance_in_the_record_and_the_proof() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet.pay_under_allowance(&request, &record).await.unwrap();
    let id = wallet.allowance(&record).await.unwrap().allowance_id;
    let mut named = false;
    for held in wallet.app().records_of("payer_activity").await.unwrap() {
        let value: Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
        if value["allowance_id"] == json!(id.as_str()) {
            named = true;
            let proof = &value["proof"]["plaintext"]["json"];
            let bytes = pubky_switch_core::enc::unb64(proof.as_str().unwrap()).unwrap();
            let proof: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(proof["body"]["allowance_id"], json!(id.as_str()));
        }
    }
    assert!(named);
}

#[tokio::test]
async fn test_executor_refuses_a_request_naming_another_allowance() {
    let w = World::new();
    let (mut shop, mut wallet, first) = set_up(&w, terms_with(json!({}))).await;
    let second = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    wallet
        .grant_executor(&second, UserConfirmed::new())
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let pending = wallet.pay_under_allowance(&request, &first).await.unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    let refused = wallet
        .pay_under_allowance(&request, &second)
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "names another Allowance"),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_refuses_a_request_in_another_asset() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut usd = draft("1", w.clock.now_ms());
    usd.amount = paykit_lib::PaymentAmount {
        value: paykit_lib::Decimal::new("1").unwrap(),
        asset: paykit_lib::Asset::new("usd").unwrap(),
    };
    let request = request_from(&mut shop, w.alice_id(), usd).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "priced in another asset"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_refuses_a_manual_only_request() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet.mark_manual_only(&request).await.unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "manual-only"), "{refused:?}");
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_executor_refuses_a_blocked_payee_on_a_fresh_read() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    // Another own app blocks bob: the executor's copy of the deny list doesn't hold him,
    // so only a read made now refuses the payment.
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new()
                .contacts()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    contacts.block(&w.bob_id()).await.unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "blocked"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pays_nothing_while_the_deny_list_doesnt_read() {
    // The executor's read of the deny list before the dispatch fails: a failed read never
    // counts as one that doesn't hold the payee, so nothing is paid; the next read pays
    // (Paykit spec 8.1, "How fresh the deny list is"; 8.3, "Executor").
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/allow".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(w.rail.all_executions().is_empty());
    wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_executor_refuses_an_ended_allowance() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet.end_allowance(&record).await.unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceEnded)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_refuses_an_expired_allowance() {
    let w = World::new();
    let expires = pubky_switch_core::time::rfc3339(w.clock.now_ms() + 60 * 60_000);
    let (mut shop, mut wallet, record) =
        set_up(&w, terms_with(json!({ "expires_at": expires }))).await;
    w.clock.advance_ms(2 * 60 * 60_000);
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "terms refuse"), "{refused:?}");
}

#[tokio::test]
async fn test_executor_refuses_over_the_per_payment_range() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "1.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "per_payment limit"), "{refused:?}");
}

#[tokio::test]
async fn test_executor_period_limit_holds_within_the_period_and_resets_after() {
    let w = World::new();
    let anchor = pubky_switch_core::time::rfc3339(w.clock.now_ms());
    let limits = json!([{
        "amount_limit": null,
        "payment_count_limit": 1,
        "period": { "kind": "anchored", "every": 1, "unit": "day", "anchor": anchor }
    }]);
    let (mut shop, mut wallet, record) =
        set_up(&w, terms_with(json!({ "period_limits": limits }))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.1").await;
    wallet.pay_under_allowance(&first, &record).await.unwrap();
    let second = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&second, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "period_count limit"), "{refused:?}");
    w.clock.advance_ms(25 * 60 * 60_000);
    let third = request(&w, &mut shop, &mut wallet, "0.1").await;
    let paid = wallet.pay_under_allowance(&third, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_executor_refuses_after_a_restore_until_confirmed_again() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let early = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&early, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "consent list"), "{refused:?}");
    // The user confirms the Allowance again: a request made after the restore is paid
    // under it.
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    let request = request_after_restore(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_executor_pays_no_request_dated_before_a_restore() {
    // A restore loses the block marks, so it owes a raise of the block floor to its first
    // read of the lists: a one-time request dated before that, plus the clock tolerance, is
    // paid only by hand (Paykit spec 8.1, "Blocked counterparties"; 8.3, "Executor"). One
    // whose record was opened before the restore is refused for the floor; one that comes
    // after the restore, dated inside the tolerance, opens its record `manual_only`. Each is
    // refused before anything is written.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let before = request(&w, &mut shop, &mut wallet, "0.5").await;
    let mut wallet = restored_confirmed(&w, wallet, &record).await;
    wallet
        .confirm_possibly_cancelled(&before, UserConfirmed::new())
        .await
        .unwrap();
    let refused = wallet
        .pay_under_allowance(&before, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "block floor"), "{refused:?}");
    let inside = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet
        .confirm_possibly_cancelled(&inside, UserConfirmed::new())
        .await
        .unwrap();
    let refused = wallet
        .pay_under_allowance(&inside, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "manual-only"), "{refused:?}");
    // Nothing was written for either: no attempt, no ledger entry, no payment.
    let (_, ledger) = held(&wallet, &record).await;
    assert!(ledger.unwrap().entries.is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_handoff_stops_the_previous_executor() {
    let w = World::new();
    let (mut shop, mut old, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    old.pay_under_allowance(&first, &record).await.unwrap();
    let mut new = executor_wallet(&w, "wallet2").await;
    new.receive(None).await.unwrap();
    new.grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let (allowance, _) = held(&new, &record).await;
    assert_eq!(allowance.grants.len(), 2);
    // The new executor held the ledger as an app that reads it, with an authenticated
    // version, so it keeps that one as its last ledger version at the handoff (Paykit spec
    // 8.3, "Last ledger version").
    assert_eq!(new.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
    let second = request(&w, &mut shop, &mut old, "0.5").await;
    let refused = old.pay_under_allowance(&second, &record).await.unwrap_err();
    assert!(
        refused_for(&refused, "another app is the Allowance's executor"),
        "{refused:?}"
    );
    // The notice takes the Allowance off the previous executor's consent list.
    old.receive(None).await.unwrap();
    assert!(!old.allowance(&record).await.unwrap().consented);
    // The new executor builds on the previous executor's ledger, the version it kept at
    // the handoff, once it passes every other check (Paykit spec 8.3, "Last ledger
    // version").
    new.receive(None).await.unwrap();
    let paid = new.pay_under_allowance(&second, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let (_, ledger) = held(&new, &record).await;
    assert_eq!(ledger.unwrap().entries.len(), 2);
    // From then on it keeps the write it built on.
    assert_eq!(new.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_executor_refuses_a_ledger_written_by_another_app() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    // Another own app holding `paykit_allowance_execute`, never the executor, writes the ledger.
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&wallet, &record).await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    other.app_mut().read_record(&target).await.unwrap();
    // Conditioned on the Allowance record as an executor's write is, so the check that
    // refuses it is the writer's: the grant history names another app at that version.
    let version = other
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .version;
    let empty = Ledger::empty(allowance.counterparty, allowance.allowance_id.clone());
    other
        .app_mut()
        .put_record(
            &target,
            &empty.to_json().unwrap(),
            vec![pubky_switch_core::ops::Condition::RecordVersion {
                key: record,
                version,
            }],
        )
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        refused_for(
            &refused,
            "the ledger's writer isn't the executor at the version it was conditioned on"
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_lost_batch_answer_counts_the_attempt_once() {
    // The executor batch is applied and its answer lost: the resume writes the attempt
    // `failed`, which releases its entry, so the limits count only what was paid.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    assert!(wallet.pay_under_allowance(&first, &record).await.is_err());
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    let (_, ledger) = held(&wallet, &record).await;
    assert_eq!(ledger.unwrap().entries.len(), 1);
    assert!(w.rail.all_executions().is_empty());
    // Two more of 1 fit the lifetime limit of 2: the failed attempt is released.
    wallet.pay_under_allowance(&first, &record).await.unwrap();
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
async fn test_executor_kept_ledger_naming_another_record_is_corrupt() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let key = format!("lv/{}", pubky_switch_core::enc::b64(&record));
    let held = wallet
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held).unwrap();
    value["d"]["record"] = json!(pubky_switch_core::enc::b64(&[9; 32]));
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused.refusal().is_none(), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pays_only_to_the_identifiers_the_terms_allow() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(
        &w,
        terms_with(json!({ "allowed_payment_endpoint_identifiers": ["btc-onchain"] })),
    )
    .await;
    // The request offers a Lightning invoice only: no endpoint the payment may use.
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_paused_after_a_provider_loss() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    // The identity lost its current generation: the grant's confirmation predates it.
    w.alice
        .next_doc(|body| body.recovery_from = Some(body.generation))
        .unwrap();
    wallet.receive(None).await.unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "provider loss"), "{refused:?}");
}

#[tokio::test]
async fn test_executor_refuses_after_a_missed_item_until_confirmed_again() {
    // A `missed` item may hide an end: the consent list is confirmed again first. A request
    // sent well after the item isn't held, so the consent alone refuses it.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60 * 60_000);
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "consent list"), "{refused:?}");
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_executor_journal_entry_with_an_allowance_that_does_not_read_is_corrupt() {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    let key = format!("j/{}", paid.attempt_id().to_text());
    let held = wallet
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut value: Value = serde_json::from_slice(&held).unwrap();
    assert!(value["d"]["allowance_id"].is_string());
    value["d"]["allowance_id"] = json!("not-an-allowance");
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    let refused = wallet.payments().await.unwrap_err();
    assert!(refused.refusal().is_none(), "{refused:?}");
}

/// Bob proposes again under the Allowance at the wallet's `record`, with other terms: a
/// second, different proposal, which makes the Allowance conflicted (Paykit spec 8.3).
async fn propose_again(w: &World, wallet: &Paykit, record: &[u8; 32]) {
    let (allowance, _) = held(wallet, record).await;
    let mut terms = terms_with(json!({ "lifetime_amount_limit": "3" }))
        .to_json()
        .unwrap();
    terms["lifetime_amount_limit"] = json!("3");
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_proposal",
        Vec::new(),
        json!({
            "allowance_id": allowance.allowance_id.as_str(),
            "proposer_role": "allowee",
            "terms": terms,
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
}

#[tokio::test]
async fn test_executor_refuses_a_conflicted_allowance() {
    // The record shows the conflict: nothing is paid under it, whatever its grant says.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    propose_again(&w, &wallet, &record).await;
    wallet.receive(None).await.unwrap();
    let (allowance, _) = held(&wallet, &record).await;
    assert!(allowance.conflicted.is_some());
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(!not_automatic(&refused), "{refused:?}");
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceConflicted)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_refuses_a_conflict_kept_before_the_record_shows_it() {
    // The second proposal is kept as the stop; the later write of it into the record is
    // refused, so the record still holds the executor's grant and no `conflicted`. The kept
    // stop alone refuses the payment.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    propose_again(&w, &wallet, &record).await;
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    let (allowance, _) = held(&wallet, &record).await;
    assert!(allowance.conflicted.is_none());
    assert_eq!(wallet.app().protocol_scan(b"cf/").await.unwrap().len(), 1);
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceConflicted)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    // The conflict took the Allowance off the consent list for good, so no grant is
    // written again.
    let grants = allowance.grants.len();
    let refused = wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(refused.refusal().is_some(), "{refused:?}");
    assert_eq!(held(&wallet, &record).await.0.grants.len(), grants);
}

#[tokio::test]
async fn test_executor_journal_entry_naming_another_allowance_is_corrupt_at_the_outcome() {
    // The executor batch goes through and the payment is in flight. The journal entry is
    // then found naming another Allowance than the one the request record names; the payment
    // completes, and the resume refuses the entry as a corrupt store before it builds the
    // proof, rather than send a proof and write a record naming the other Allowance.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let _ = wallet.pay_under_allowance(&first, &record).await;
    let executed: Vec<_> = w.rail.all_executions().into_keys().collect();
    assert_eq!(executed.len(), 1);
    let (key, kept) = wallet.app().protocol_scan(b"j/").await.unwrap().remove(0);
    let mut value: Value = serde_json::from_slice(&kept).unwrap();
    assert!(value["d"]["allowance_id"].is_string());
    value["d"]["allowance_id"] = json!("123e4567-e89b-42d3-a456-426614174000");
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key,
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    w.rail.settle(&executed[0], true);
    // The outcome isn't written: no proof is signed or sent, and the entry stays kept.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    assert_eq!(wallet.app().protocol_scan(b"j/").await.unwrap().len(), 1);
    assert!(wallet
        .requests()
        .await
        .unwrap()
        .iter()
        .all(|view| view.status != paykit_sdk::RequestStatus::Paid));
}

/// `wallet` backed up and restored, reconciled, read, and its user's confirmations after
/// the restore given for `record` and `request`.
async fn restored_confirmed(w: &World, wallet: Paykit, record: &[u8; 32]) -> Paykit {
    let mut wallet = wallet;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .confirm_allowance_again(record, UserConfirmed::new())
        .await
        .unwrap();
    wallet
}

#[tokio::test]
async fn test_executor_restored_before_its_empty_ledger_lands_takes_none_as_lost() {
    // The grant goes through and the empty ledger's write can't reach the switch. The
    // executor is restored from a backup taken then: its journal, not restored, no longer
    // shows the empty ledger not yet sent, so the missing ledger is usage history lost, and
    // nothing is paid automatically (Paykit spec 8.3, "A grant implies a ledger").
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 1,
        });
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let (allowance, ledger) = held(&wallet, &record).await;
    assert_eq!(allowance.grants.len(), 1);
    assert!(ledger.is_none());
    let mut wallet = restored_confirmed(&w, wallet, &record).await;
    let request = request_after_restore(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "usage history lost"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_first_grant_over_a_ledger_at_its_key_writes_no_empty_one() {
    // Another own app wrote a ledger at the Allowance's key before the first grant: the
    // executor writes no empty ledger over it, and takes it as one it didn't accept, so it
    // pays nothing automatically (Paykit spec 8.3, "A grant implies a ledger").
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&wallet, &record).await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    other.app_mut().read_record(&target).await.unwrap();
    let left = Ledger::empty(allowance.counterparty, allowance.allowance_id.clone());
    other
        .app_mut()
        .put_record(&target, &left.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let writes = w.record_writes(&w.alice, &wallet.app().record_key(&target).unwrap());
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        w.record_writes(&w.alice, &wallet.app().record_key(&target).unwrap()),
        writes
    );
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "isn't one this executor accepted"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pauses_under_an_ignored_write_until_the_user_confirms() {
    // Another own app writes the Allowance record without the acceptance the executor saw:
    // the executor ignores that write and pays nothing automatically under the Allowance
    // until its user confirms it; the confirmation covers that write only (Paykit spec 8.3,
    // "Executor").
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    let mut dropped: Value = serde_json::from_slice(&allowance.to_json().unwrap()).unwrap();
    dropped.as_object_mut().unwrap().remove("response");
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::collections::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    other
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&dropped).unwrap(), Vec::new())
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    assert!(
        wallet
            .app()
            .shared_record(&record)
            .await
            .unwrap()
            .unwrap()
            .flagged
    );
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "ignores"), "{refused:?}");
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

/// The ledger's key at the wallet, for scripting the switch's answers to its reads.
async fn ledger_key(wallet: &Paykit, record: &[u8; 32]) -> [u8; 32] {
    let (allowance, _) = held(wallet, record).await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    wallet.app().record_key(&target).unwrap()
}

#[tokio::test]
async fn test_executor_missing_ledger_after_its_own_ledger_write_is_usage_history_lost() {
    // The first grant's empty ledger can't reach the switch, so its batch is kept; the next
    // write sends it, and it lands with no version kept from it. The switch then answers
    // `404` for the ledger: after its own ledger write was sent, the executor takes that as
    // "usage history lost" (Paykit spec 8.3, "A grant implies a ledger"), never as an empty
    // ledger with room under the lifetime limit.
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(
        &w,
        &mut shop,
        &mut wallet,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    )
    .await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 1,
        });
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    assert!(held(&wallet, &record).await.1.is_none());
    // The kept batch goes with the next read, and lands.
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    let key = ledger_key(&wallet, &record).await;
    assert!(w.record_writes(&w.alice, &key) > 0);
    // The switch hides the ledger.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: format!("records/{}", pubky_switch_core::enc::b64(&key)),
            code: pubky_switch_core::http::ErrorCode::NotFound,
            current: Value::Null,
        });
    let refused = wallet
        .pay_under_allowance(&first, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "usage history lost"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pays_nothing_under_an_allowance_record_deleted_by_another_own_app() {
    // Another own app deletes the accepted Allowance's record: the executor holds it as
    // deleted and pays nothing automatically under it.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let (allowance, _) = held(&wallet, &record).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let target = pubky_switch_client::app::RecordTarget::new(
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
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused.refusal().is_some(), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_empty_ledger_owed_after_a_kept_grant_written_before_the_first_payment() {
    // The grant's batch can't reach the switch, so it is kept and the empty ledger isn't
    // written after it. The kept batch lands with the next read, the empty ledger still
    // owed: the first payment writes it first, then pays on it (Paykit spec 8.3, "A grant
    // implies a ledger").
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    let (allowance, ledger) = held(&wallet, &record).await;
    assert_eq!(allowance.grants.len(), 1);
    assert!(ledger.is_none());
    let key = ledger_key(&wallet, &record).await;
    assert_eq!(w.record_writes(&w.alice, &key), 0);
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // The empty ledger, then the payment's entry.
    assert_eq!(w.record_writes(&w.alice, &key), 2);
    let (_, ledger) = held(&wallet, &record).await;
    assert_eq!(ledger.unwrap().entries.len(), 1);
}

#[tokio::test]
async fn test_executor_claim_read_failing_writes_nothing() {
    // The empty ledger is still owed when the executor's read of the occurrence's claim,
    // for the attempts it holds, fails: the read is the error, and it comes before anything
    // is written, so the empty ledger stays owed and nothing is paid. The next payment
    // writes it, then pays on it (Paykit spec 8.3, "Doubtful obligations", "A grant implies
    // a ledger").
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    let key = ledger_key(&wallet, &record).await;
    assert_eq!(w.record_writes(&w.alice, &key), 0);
    // The claim is read once for the attempt's step, then once for its attempts.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/claims/".into(),
            after: 1,
        });
    assert!(wallet.pay_under_allowance(&first, &record).await.is_err());
    assert_eq!(w.record_writes(&w.alice, &key), 0);
    assert!(w.rail.all_executions().is_empty());
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.record_writes(&w.alice, &key), 2);
}

#[tokio::test]
async fn test_executor_pays_inside_its_limits_until_ended_through_a_restore() {
    // One Allowance from acceptance to end. Bob proposes under a lifetime limit of 1 btc and
    // the wallet accepts it and becomes its executor. It pays a request inside the limit, and
    // refuses one the limit has no room for. Restored from a backup, it pays nothing until
    // its user confirms the Allowance again, then pays inside the limit once more. Bob ends
    // the Allowance: from then on nothing is paid under it (Paykit spec 8.3).
    let w = World::new();
    let (mut shop, mut wallet, record) =
        set_up(&w, terms_with(json!({ "lifetime_amount_limit": "1" }))).await;
    let inside = request(&w, &mut shop, &mut wallet, "0.4").await;
    let paid = wallet.pay_under_allowance(&inside, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let outside = request(&w, &mut shop, &mut wallet, "0.7").await;
    let refused = wallet
        .pay_under_allowance(&outside, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    // A restore: nothing is paid until the user confirms the Allowance again.
    let held_back = request(&w, &mut shop, &mut wallet, "0.3").await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_under_allowance(&held_back, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "consent list"), "{refused:?}");
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    let after = request_after_restore(&w, &mut shop, &mut wallet, "0.3").await;
    let paid = wallet.pay_under_allowance(&after, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
    // Bob ends it: nothing more is paid under it.
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.end_allowance(&theirs).await.unwrap();
    let last = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&last, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceEnded)),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 2);
}

/// An Allowance granted to an executor wallet on an idempotent rail without history, with
/// a request for 0.5 btc paid under it and the dispatch left in flight. Returns bob's shop,
/// the wallet, the wallet's record key and the attempt.
async fn executor_in_flight(w: &World) -> (Paykit, Paykit, [u8; 32], paykit_lib::AttemptId) {
    let (shop, wallet, record, attempt, _) = Box::pin(executor_in_flight_on(w, true)).await;
    (shop, wallet, record, attempt)
}

/// [`executor_in_flight`] on a rail that is idempotent on the attempt ID or not, with the
/// request paid.
async fn executor_in_flight_on(
    w: &World,
    idempotent: bool,
) -> (Paykit, Paykit, [u8; 32], paykit_lib::AttemptId, RequestRef) {
    w.rail.set_declaration(
        paykit_sdk::rail::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(idempotent),
    );
    let mut shop = allowance_shop(w).await;
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new()
                .pay()
                .allowance()
                .allowance_execute()
                .contacts()
                .build()
                .unwrap(),
            crate::common::day_window(),
            crate::common::store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let record = accepted(w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let request = request(w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        wallet.pay_under_allowance(&request, &record).await.unwrap()
    else {
        panic!("in flight");
    };
    (shop, wallet, record, attempt_id, request)
}

/// The rail's calls for `attempt` once `wallet` resumes a minute later.
async fn calls_after_resume(
    w: &World,
    wallet: &mut Paykit,
    attempt: &paykit_lib::AttemptId,
) -> u64 {
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    w.rail.pay_calls(attempt)
}

#[tokio::test]
async fn test_executor_repeat_goes_under_an_allowance_still_in_force() {
    // The control: nothing changed since the dispatch, so the resume repeats it on the
    // idempotent method (Paykit spec 7.2).
    let w = World::new();
    let (_shop, mut wallet, _, attempt) = executor_in_flight(&w).await;
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 2);
}

#[tokio::test]
async fn test_executor_repeat_held_under_an_allowance_ended_since() {
    // Bob ends the Allowance while the dispatch is in flight. The resume can't tell a
    // dispatch that reached the method from one stopped before its call, so the repeat
    // holds on the end the Allowance record shows: the method isn't called again, the
    // attempt stays open, and nothing new is paid under the ended Allowance (Paykit spec 7.2,
    // 8.3, "Ending").
    let w = World::new();
    let (mut shop, mut wallet, record, attempt) = executor_in_flight(&w).await;
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
    let next = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&next, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceEnded)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_repeat_finishes_a_dispatch_under_an_allowance_conflicted_since() {
    // Bob proposes again under the Allowance ID while the dispatch is in flight: the
    // Allowance is conflicted, and one already dispatched finishes (Paykit spec 8.3,
    // "Conflicted"): the resume repeats it with the same idempotency key.
    let w = World::new();
    let (mut shop, mut wallet, record, attempt) = executor_in_flight(&w).await;
    propose_again(&w, &wallet, &record).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 2);
    let next = request(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&next, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceConflicted)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_executor_no_repeat_under_an_allowance_on_a_method_without_idempotent_repeats() {
    // The method isn't idempotent on the attempt ID: no repeat is made, Allowance in force
    // or not, and the attempt stays "may be paid" (Paykit spec 7.2, "What a payment method
    // declares").
    let w = World::new();
    let (mut shop, mut wallet, _, attempt, _) = executor_in_flight_on(&w, false).await;
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
}

#[tokio::test]
async fn test_executor_no_repeat_while_the_allowance_needs_confirming_again() {
    // A `missed` item while the dispatch is in flight: every Allowance on the consent list
    // is to be confirmed again, so the method isn't called again until the user does, also
    // once the user has seen that the request may have been cancelled (Paykit spec 7.2,
    // 8.3, "After a restore").
    let w = World::new();
    let (_shop, mut wallet, record, attempt, request) = executor_in_flight_on(&w, true).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
    // Once the user confirms the Allowance again, the repeat goes.
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 2);
}

#[tokio::test]
async fn test_executor_restored_with_an_older_ledger_version_builds_on_the_newer_one() {
    // The executor pays 0.4 under a lifetime limit of 1, is backed up, then pays 0.4 more.
    // Restored, it keeps the older ledger version; the ledger shown is built on it and
    // counts no less, so the executor accepts it and counts both payments: after its user
    // confirms again, a third of 0.4 has no room under the limit, and 0.1 still does (Paykit
    // spec 8.3, "Last ledger version", "From one version to the next").
    let w = World::new();
    let (mut shop, mut wallet, record) =
        set_up(&w, terms_with(json!({ "lifetime_amount_limit": "1" }))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.4").await;
    wallet.pay_under_allowance(&first, &record).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let second = request(&w, &mut shop, &mut wallet, "0.4").await;
    wallet.pay_under_allowance(&second, &record).await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    // The restore's block floor rises to the time the lists are first read after it
    // (Paykit spec 8.1, "Blocked counterparties"): they are read before the requests below.
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.receive(None).await.unwrap();
    let third = request_after_restore(&w, &mut shop, &mut wallet, "0.4").await;
    let fourth = request_after_restore(&w, &mut shop, &mut wallet, "0.1").await;
    let refused = wallet
        .pay_under_allowance(&third, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "lifetime limit"), "{refused:?}");
    let paid = wallet.pay_under_allowance(&fourth, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let (_, ledger) = held(&wallet, &record).await;
    assert_eq!(ledger.unwrap().entries.len(), 3);
}

#[tokio::test]
async fn test_executor_missing_ledger_after_its_first_payment_write_is_usage_history_lost() {
    // The first grant's empty ledger is refused, so the first payment's own batch writes the
    // ledger, and money moves; that write is kept as the last version from the copy its
    // answer leaves, with no read. The switch then answers `404` for the ledger: the
    // executor pays nothing over the lifetime limit.
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(
        &w,
        &mut shop,
        &mut wallet,
        terms_with(json!({ "lifetime_amount_limit": "1" })),
    )
    .await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let first = request(&w, &mut shop, &mut wallet, "1").await;
    let key = ledger_key(&wallet, &record).await;
    let path = format!("records/{}", pubky_switch_core::enc::b64(&key));
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: Value::Null,
        });
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.record_writes(&w.alice, &key), 1);
    assert_eq!(wallet.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path,
            code: pubky_switch_core::http::ErrorCode::NotFound,
            current: Value::Null,
        });
    let second = request(&w, &mut shop, &mut wallet, "1").await;
    let refused = wallet
        .pay_under_allowance(&second, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "usage history lost"), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// The target of the wallet's Allowance record of `allowance`.
fn allowances_target(allowance: &AllowanceRecord) -> pubky_switch_client::app::RecordTarget {
    pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowances",
            paykit_lib::collections::Collection::Allowances.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    )
}

#[tokio::test]
async fn test_executor_confirmation_never_covers_a_removed_apps_write() {
    // Another own app writes the Allowance record without its acceptance, and the wallet
    // ignores that write. The app is then removed with a `distrust_from` before that write,
    // so the write counts for nothing by its writer's standing either. The user's
    // confirmation covers no such write: no `fc/` is kept, and nothing is paid under the
    // Allowance while that write is current (Paykit spec 8.3, "Building on a version
    // accepts it").
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    let before = w.clock.now_ms();
    w.clock.advance_ms(60_000);
    let mut dropped: Value = serde_json::from_slice(&allowance.to_json().unwrap()).unwrap();
    dropped.as_object_mut().unwrap().remove("response");
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &serde_json::to_vec(&dropped).unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let other_id = other.app_id();
    w.alice
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != other_id);
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: other_id,
                distrust_from: before,
            });
        })
        .unwrap();
    w.clock.advance_ms(1_000);
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().own_identity().lists_removed(&other_id));
    assert!(
        wallet
            .app()
            .shared_record(&record)
            .await
            .unwrap()
            .unwrap()
            .flagged
    );
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    assert!(wallet.app().protocol_scan(b"fc/").await.unwrap().is_empty());
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "ignores"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_confirmation_of_an_ignored_write_isnt_restored() {
    // The user confirmed paying under an ignored current write; the executor is restored
    // from a backup taken then. The confirmation isn't carried: nothing is paid until the
    // user confirms again after the restore (Paykit spec 8.3, "After a restore").
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    let mut dropped: Value = serde_json::from_slice(&allowance.to_json().unwrap()).unwrap();
    dropped.as_object_mut().unwrap().remove("response");
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &serde_json::to_vec(&dropped).unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(wallet.app().protocol_scan(b"fc/").await.unwrap().len(), 1);
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"fc/").await.unwrap().is_empty());
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(not_automatic(&refused), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_refuses_a_removed_apps_record_write_taken_before_its_removal() {
    // Another own app rewrites the Allowance record as it stands; the wallet takes the write.
    // The app is then removed with a `distrust_from` before that write: nothing is paid while
    // that write is current.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    let before = w.clock.now_ms();
    w.clock.advance_ms(60_000);
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &allowance.to_json().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let shared = wallet.app().shared_record(&record).await.unwrap().unwrap();
    assert_eq!(shared.writer, other.app_id());
    assert!(!shared.flagged);
    let other_id = other.app_id();
    w.alice
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != other_id);
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: other_id,
                distrust_from: before,
            });
        })
        .unwrap();
    w.clock.advance_ms(1_000);
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(not_automatic(&refused), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_restored_over_a_ledger_it_did_not_accept_pays_nothing() {
    // A ledger was at its key at the first grant; the executor is restored, its empty-ledger
    // mark not restored: it still pays nothing on that ledger.
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let record = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    let mut other = executor_wallet(&w, "other").await;
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&wallet, &record).await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    other.app_mut().read_record(&target).await.unwrap();
    let left = Ledger::empty(allowance.counterparty, allowance.allowance_id.clone());
    other
        .app_mut()
        .put_record(&target, &left.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "before the first grant"),
        "{refused:?}"
    );
    let mut wallet = restored_confirmed(&w, wallet, &record).await;
    let request = request_after_restore(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(not_automatic(&refused), "{refused:?}");
    assert!(
        !refused_for(&refused, "block floor") && !refused_for(&refused, "held"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_handoff_notice_not_delivered_goes_at_the_next_flush() {
    // The new executor's grant is applied with its handoff notice queued in the batch; the
    // try to deliver the notice then fails at the switch. A send that fails is no error: the
    // grant goes on, and the notice, still queued, goes at the next flush and takes the
    // Allowance off the previous executor's consent list.
    let w = World::new();
    let (mut shop, mut old, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    old.pay_under_allowance(&first, &record).await.unwrap();
    let mut new = executor_wallet(&w, "wallet2").await;
    new.receive(None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "inbox".into(),
            after: 0,
        });
    new.grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    let (allowance, _) = held(&new, &record).await;
    assert_eq!(allowance.grants.len(), 2);
    old.receive(None).await.unwrap();
    assert!(old.allowance(&record).await.unwrap().consented);
    new.app_mut().flush_outbox().await.unwrap();
    old.receive(None).await.unwrap();
    assert!(!old.allowance(&record).await.unwrap().consented);
}

/// Another own app blocks bob, or the wallet's read of the lists fails (`blocked`), and the
/// wallet, the executor, then ends the Allowance it allows. A user's end is never held: it
/// is recorded and sent at once, and once bob is unblocked, or the lists read again, the
/// executor pays nothing under the Allowance (Paykit spec 8.1, "Blocked counterparties";
/// 8.3, "Ending").
async fn ended_while_sends_held(blocked: bool) {
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new()
                .contacts()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    if blocked {
        contacts.block(&w.bob_id()).await.unwrap();
    } else {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: Value::Null,
            });
    }
    wallet.receive(None).await.unwrap();
    let expected = if blocked { Some(true) } else { None };
    assert_eq!(wallet.allowance(&record).await.unwrap().blocked, expected);
    wallet.end_allowance(&record).await.unwrap();
    assert_eq!(
        wallet.allowance(&record).await.unwrap().status,
        AllowanceStatus::Ended
    );
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    assert_eq!(
        shop.allowance(&theirs).await.unwrap().status,
        AllowanceStatus::Ended
    );
    if blocked {
        contacts.unblock(&w.bob_id()).await.unwrap();
    }
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceEnded)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_end_to_a_blocked_counterparty_sent_and_pays_nothing_after() {
    ended_while_sends_held(true).await;
}

#[tokio::test]
async fn test_executor_end_while_the_deny_copy_is_unknown_sent_and_pays_nothing_after() {
    ended_while_sends_held(false).await;
}

#[tokio::test]
async fn test_executor_handoff_notice_store_failure_is_the_grants_error() {
    // The new executor's store fails the commit that records the handoff notice delivered:
    // the grant returns that failure, and nothing goes on past it.
    let w = World::new();
    let (mut shop, mut old, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut old, "0.5").await;
    old.pay_under_allowance(&first, &record).await.unwrap();
    let base = crate::common::store();
    let fault = std::sync::Arc::new(pubky_switch_client::store::FaultStore::new(
        std::sync::Arc::clone(&base),
        pubky_switch_client::store::Fault::None,
    ));
    let mut new = w
        .join_with(
            &w.alice,
            "wallet2",
            AppScopes::new()
                .pay()
                .allowance()
                .allowance_execute()
                .contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&fault)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    new.app_mut().invite(&w.bob_id()).await.unwrap();
    new.receive(None).await.unwrap();
    fault.fail_put_once(b"sent/");
    let granted = new.grant_executor(&record, UserConfirmed::new()).await;
    assert!(granted.is_err(), "{granted:?}");
}

/// How many `allowance_end`s `app` takes at one receive.
async fn ends_taken(app: &mut Paykit) -> usize {
    app.receive(None)
        .await
        .unwrap()
        .report
        .events
        .iter()
        .filter(|e| {
            matches!(e, pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.allowance_end")
        })
        .count()
}

/// The conformance case `crossing_end_recorded_while_held` (Paykit spec 8.3, "Ending"): alice,
/// the allower, has two apps, W, where her user answers proposals, and X, her executor; bob,
/// the allowee, has his shop S. S proposes, W accepts and X grants; S withdraws before it has
/// the acceptance, and sends to bob are held at W (`blocked`: alice's user blocks bob;
/// otherwise W's read of the lists fails) before W reads the withdrawal.
/// (a) W reads it: the record holds W's end naming the proposal and the acceptance, X is sent
/// the executor notice, and nothing reaches S.
/// (b) Sends aren't held any more, and X runs before W runs again, with an open request the
/// Allowance would cover: X pays nothing. At W's next run the end's message reaches S, once.
async fn crossing_end_recorded_while_held(blocked: bool) {
    let w = World::new();
    let mut s = allowance_shop(&w).await;
    let mut wapp = w
        .join(
            &w.alice,
            "answers",
            AppScopes::new()
                .pay()
                .allowance()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    wapp.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut x = executor_wallet(&w, "executor").await;
    let sent = s
        .propose_allowance(w.alice_id(), AllowanceRole::Allowee, terms_with(json!({})))
        .await
        .unwrap();
    wapp.receive(None).await.unwrap();
    let record = wapp.allowances().await.unwrap()[0].record;
    wapp.accept_allowance(&record).await.unwrap();
    x.receive(None).await.unwrap();
    x.grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    assert!(x.allowance(&record).await.unwrap().consented);
    wapp.receive(None).await.unwrap();
    // S withdraws before it has the acceptance.
    s.end_allowance(&sent.record).await.unwrap();
    if blocked {
        wapp.block(&w.bob_id()).await.unwrap();
    } else {
        w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: Value::Null,
            });
    }
    // (a)
    wapp.receive(None).await.unwrap();
    let (held, _) = held(&wapp, &record).await;
    let end = held.end.as_ref().expect("the end is recorded at once");
    assert_eq!(end.sender(), w.alice_id());
    assert_eq!(
        end.message().refs,
        vec![held.proposal.hash(), held.response.as_ref().unwrap().hash()]
    );
    assert_eq!(wapp.app().protocol_scan(b"eo/").await.unwrap().len(), 1);
    assert_eq!(ends_taken(&mut s).await, 0);
    // X is sent the executor notice: it takes exactly one at its next receive.
    let notices = x
        .receive(None)
        .await
        .unwrap()
        .report
        .events
        .iter()
        .filter(|e| {
            matches!(e, pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.executor_notice")
        })
        .count();
    assert_eq!(notices, 1);
    assert!(!x.allowance(&record).await.unwrap().consented);
    // (b)
    if blocked {
        wapp.unblock(&w.bob_id()).await.unwrap();
    }
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    let open = request(&w, &mut s, &mut x, "0.5").await;
    let refused = x.pay_under_allowance(&open, &record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AllowanceEnded)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(ends_taken(&mut s).await, 0);
    wapp.receive(None).await.unwrap();
    assert_eq!(ends_taken(&mut s).await, 1);
    wapp.receive(None).await.unwrap();
    assert_eq!(ends_taken(&mut s).await, 0);
    assert!(wapp.app().protocol_scan(b"eo/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_executor_crossing_end_recorded_while_blocked_stops_a_separate_executor() {
    crossing_end_recorded_while_held(true).await;
}

#[tokio::test]
async fn test_executor_crossing_end_recorded_while_the_deny_copy_is_unknown_stops_a_separate_executor(
) {
    crossing_end_recorded_while_held(false).await;
}

/// Alice's answer to bob's open request, made while sends to bob are held (`blocked`: alice's
/// user blocks him; otherwise the answering app's copy of the deny list isn't known), at the
/// executor itself or, with `other_app`, at another own app: a rejection or, with `cancel`, a
/// payer's cancellation. The answer is recorded at once and only its message waits (Paykit
/// spec 8.1, "Blocked counterparties"), so once sends aren't held any more the executor pays
/// nothing under the Allowance for that request.
async fn held_answer_then_executor(blocked: bool, cancel: bool, other_app: bool) {
    let w = World::new();
    let (mut shop, mut x, record) = set_up(&w, terms_with(json!({}))).await;
    let open = request(&w, &mut shop, &mut x, "0.5").await;
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new()
                .contacts()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let mut other = if other_app {
        let mut app = w
            .join(
                &w.alice,
                "answers",
                AppScopes::new().pay().contacts().build().unwrap(),
            )
            .await;
        app.app_mut().invite(&w.bob_id()).await.unwrap();
        app.receive(None).await.unwrap();
        Some(app)
    } else {
        None
    };
    let answering = other.as_mut().unwrap_or(&mut x);
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    let fail_read = || {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: Value::Null,
            });
    };
    if blocked {
        contacts.block(&w.bob_id()).await.unwrap();
        answering.receive(None).await.unwrap();
        assert_eq!(
            answering.requests().await.unwrap()[0].status,
            RequestStatus::Blocked
        );
    } else {
        fail_read();
        answering.receive(None).await.unwrap();
        fail_read();
    }
    if cancel {
        answering.cancel_request(&open, None).await.unwrap();
    } else {
        answering.reject_request(&open, None).await.unwrap();
    }
    if blocked {
        contacts.unblock(&w.bob_id()).await.unwrap();
    }
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    x.receive(None).await.unwrap();
    let refused = x.pay_under_allowance(&open, &record).await.unwrap_err();
    let expected = if cancel {
        Refusal::Cancelled
    } else {
        Refusal::Rejected
    };
    assert_eq!(
        refused.refusal(),
        Some(&expected),
        "blocked {blocked} cancel {cancel} other_app {other_app}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pays_nothing_for_a_request_rejected_while_its_payee_was_blocked() {
    held_answer_then_executor(true, false, false).await;
    held_answer_then_executor(true, false, true).await;
}

#[tokio::test]
async fn test_executor_pays_nothing_for_a_request_rejected_while_the_deny_copy_was_unknown() {
    held_answer_then_executor(false, false, false).await;
    held_answer_then_executor(false, false, true).await;
}

#[tokio::test]
async fn test_executor_pays_nothing_for_a_request_cancelled_while_its_payee_was_blocked() {
    held_answer_then_executor(true, true, false).await;
    held_answer_then_executor(true, true, true).await;
}

#[tokio::test]
async fn test_executor_pays_nothing_for_a_request_cancelled_while_the_deny_copy_was_unknown() {
    held_answer_then_executor(false, true, false).await;
    held_answer_then_executor(false, true, true).await;
}

/// Alice's executor wallet as [`executor_wallet`], on `store`, its requests going through
/// `transport`.
async fn executor_wallet_through(
    w: &World,
    store: Arc<dyn StateStore>,
    transport: Arc<dyn Transport>,
) -> Paykit {
    let mut app = w
        .join_through(
            &w.alice,
            "wallet",
            AppScopes::new()
                .pay()
                .allowance()
                .allowance_execute()
                .contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            store,
            transport,
        )
        .await;
    app.app_mut().invite(&w.bob_id()).await.unwrap();
    app
}

/// [`set_up`], the wallet on `store`, its requests going through `transport`.
async fn set_up_through(
    w: &World,
    store: Arc<dyn StateStore>,
    transport: Arc<dyn Transport>,
) -> (Paykit, Paykit, [u8; 32]) {
    let mut shop = Box::pin(allowance_shop(w)).await;
    let mut wallet = Box::pin(executor_wallet_through(w, store, transport)).await;
    let record = Box::pin(accepted(w, &mut shop, &mut wallet, terms_with(json!({})))).await;
    wallet
        .grant_executor(&record, UserConfirmed::new())
        .await
        .unwrap();
    (shop, wallet, record)
}

/// [`set_up_through`], the wallet on `store`.
async fn set_up_on(w: &World, store: Arc<dyn StateStore>) -> (Paykit, Paykit, [u8; 32]) {
    let router = Arc::clone(&w.router) as Arc<dyn Transport>;
    Box::pin(set_up_through(w, store, router)).await
}

/// The service's mark of an upload, kept as divergent for a record.
const UPLOADER: [u8; 16] = [0x5b; 16];

#[tokio::test]
async fn test_executor_pays_nothing_under_an_allowance_record_seen_divergent_until_confirmed() {
    // After a provider loss the Allowance record's view shows it divergent: its upload may
    // hold an end or a rejection the current write lacks. The executor pays nothing under
    // it, and once a newer write clears the view it still pays nothing until the user
    // confirms the record (Paykit spec 6, "Divergent records"; 8.3, "Executor").
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let mut other = w
        .join(
            &w.alice,
            "other",
            AppScopes::new().allowance().build().unwrap(),
        )
        .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    w.alice
        .switch()
        .make_divergent(UPLOADER, record, [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    let refused = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "divergent or untrusted"),
        "{refused:?}"
    );
    // Another own app writes the record again, unchanged: the view isn't divergent now.
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &allowance.to_json().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(
        !wallet
            .app()
            .shared_record(&record)
            .await
            .unwrap()
            .unwrap()
            .divergent
    );
    let refused = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "seen divergent"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let paid = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_executor_pays_nothing_under_a_ledger_seen_divergent() {
    // The ledger record's view shows it divergent: its upload may hold entries the current
    // write lacks, so the room left under the limits isn't known. The executor pays nothing
    // under the Allowance, and the ledger is shown among the divergent records, for the user
    // to confirm (Paykit spec 6, "Divergent records"; 8.3, "Building on a version accepts
    // it").
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let ledger = ledger_key(&wallet, &record).await;
    w.alice
        .switch()
        .make_divergent(UPLOADER, ledger, [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    let refused = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "the ledger is divergent, or was seen so"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let shown = wallet.divergent_records().await.unwrap();
    assert!(
        shown.iter().any(|shown| shown.record == ledger
            && shown.collection == paykit_lib::collections::Collection::AllowanceLedgers),
        "{shown:?}"
    );
}

#[tokio::test]
async fn test_executor_pays_nothing_while_an_answer_is_kept_to_write() {
    // Alice's wallet proposed as allower, and bob accepted. An answer of bob's to the
    // proposal kept before the record and not yet written (`pr/`) may be an earlier
    // different answer, which takes the response or makes the Allowance conflicted once it
    // is written: while it is kept the executor pays nothing under the Allowance (Paykit
    // spec 8.1, "Replies before their record"; 8.3, "Conflicted").
    let w = World::new();
    let mut shop = Box::pin(allowance_shop(&w)).await;
    let mut wallet = Box::pin(executor_wallet(&w, "wallet")).await;
    let sent = wallet
        .propose_allowance(w.bob_id(), AllowanceRole::Allower, terms_with(json!({})))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.accept_allowance(&theirs).await.unwrap();
    // The switch refuses the acceptance's write: it is kept to write.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::TooLarge,
            current: Value::Null,
        });
    wallet.receive(None).await.unwrap();
    let mut kept = wallet.app().protocol_scan(b"pr/").await.unwrap();
    assert_eq!(kept.len(), 1);
    let (key, value) = kept.remove(0);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    assert_eq!(
        wallet.allowance(&sent.record).await.unwrap().status,
        AllowanceStatus::Accepted
    );
    wallet
        .grant_executor(&sent.record, UserConfirmed::new())
        .await
        .unwrap();
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    // The answer is kept to write again, as it was before its write.
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put { key, value }])
        .await
        .unwrap();
    let refused = Box::pin(wallet.pay_under_allowance(&request, &sent.record))
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "an answer to the Allowance's proposal is kept"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pays_nothing_while_its_counterparty_owes_a_resend() {
    // Bob's replies kept before their records pass the bound on one sender's kept replies:
    // the earliest is dropped, and bob owes a re-send (`pd/`). An answer to the Allowance's
    // proposal may be among the dropped replies, so while the re-send is owed the executor
    // pays nothing under an Allowance of bob's (Paykit spec 8.1, "Replies before their
    // record"; 8.3, "Conflicted").
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    for i in 0..=paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        w.clock.advance_ms(1);
        w.inject(
            &w.bob,
            "shop",
            &w.alice,
            &[wallet.app_id()],
            "paykit.allowance_acceptance",
            vec![[u8::try_from(i).unwrap(); 32]],
            json!({ "allowance_id": paykit_lib::AllowanceId::generate().unwrap().as_str() })
                .as_object()
                .cloned()
                .unwrap(),
        );
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"pd/").await.unwrap().len(), 1);
    let refused = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "owes a re-send"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_executor_pays_nothing_when_the_resend_mark_does_not_read() {
    // The mark of a re-send bob owes holds a value that doesn't read: a corrupt store, never
    // no mark, so the executor pays nothing (Paykit spec 8.1, "Replies before their record").
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let bob = pubky_switch_core::enc::b64(&w.bob_id());
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: format!("pd/{bob}").into_bytes(),
            value: serde_json::to_vec(&json!({ "v": 1, "d": { "sender": bob } })).unwrap(),
        }])
        .await
        .unwrap();
    let failed = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    assert!(w.rail.all_executions().is_empty());
}

const MINUTE_MS: u64 = 60_000;

/// A store that notes the commit that journals an attempt's `Paying`, and runs a hook once,
/// during it: after the checks before the commit and before the payment method's call. A
/// second hook runs once during the first commit that journals an attempt `Attempted`: after
/// the attempt's batch is applied and before the final payment check. Set to, it fails the
/// first commit that journals an attempt `Attempted` after its `Paying`, applying nothing, as
/// a process stopping before it would.
#[derive(Default)]
struct PayingStore {
    inner: pubky_switch_client::store::MemoryStore,
    paying: std::sync::atomic::AtomicBool,
    hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    attempted_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    fail_write_back: std::sync::atomic::AtomicBool,
}

impl PayingStore {
    fn on_paying(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn on_attempted(&self, hook: impl FnOnce() + Send + 'static) {
        *self.attempted_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Fail the write back to `Attempted` after the attempt's `Paying` commit, once.
    fn fail_the_write_back(&self) {
        self.fail_write_back
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether an attempt's `Paying` was committed.
    fn paying_seen(&self) -> bool {
        self.paying.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl StateStore for PayingStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        const PAYING: &[u8] = b"\"stage\":\"paying\"";
        const ATTEMPTED: &[u8] = b"\"stage\":\"attempted\"";
        let journals = |stage: &[u8]| {
            batch.writes().iter().any(|write| {
                matches!(write, pubky_switch_client::store::Write::Put { key, value, .. }
                    if key.starts_with(b"p/j/") && value.windows(stage.len()).any(|w| w == stage))
            })
        };
        let (paying, attempted) = (journals(PAYING), journals(ATTEMPTED));
        if attempted
            && self.paying_seen()
            && self
                .fail_write_back
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.commit(batch)?;
        if attempted {
            if let Some(hook) = self.attempted_hook.lock().unwrap().take() {
                hook();
            }
        }
        if paying {
            self.paying.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(hook) = self.hook.lock().unwrap().take() {
                hook();
            }
        }
        Ok(())
    }
}

/// Another payer app of alice's, holding `paykit_pay` and `paykit_manage_contacts`.
async fn keeper(w: &World) -> Paykit {
    let mut keeper = w
        .join(
            &w.alice,
            "keeper",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
        )
        .await;
    keeper.app_mut().invite(&w.carol_id()).await.unwrap();
    keeper
}

/// [`keeper`], which drops carol's request for the block and unblocks her: its next
/// receive, once the lists are read again, asks her for the request and raises the block
/// floor past every request made before it.
async fn keeper_about_to_raise_the_floor(w: &World) -> Paykit {
    let mut keeper = Box::pin(keeper(w)).await;
    let mut till = w
        .join(
            &w.carol,
            "till",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    keeper.app_mut().deny(&w.carol_id()).await.unwrap();
    keeper.receive(None).await.unwrap();
    keeper.unblock(&w.carol_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    keeper
}

/// Run `keeper`'s receive on its own thread, which raises the floor, while the caller waits.
fn raise_the_floor(w: &World, mut keeper: Paykit) {
    let carol = w.carol_id();
    let asked = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                keeper.receive(None).await.unwrap();
                crate::common::asked(&keeper, &carol).await.len()
            })
    })
    .join()
    .unwrap();
    assert_eq!(asked, 1);
}

/// The executor's payment of `request` under `record`, held at its dispatch: nothing
/// reaches the payment method, the outcome is pending, the ledger holds the attempt's entry
/// from the batch written before the dispatch, and the attempt is known unsubmitted.
async fn held_at_the_dispatch(
    w: &World,
    wallet: &mut Paykit,
    request: &RequestRef,
    record: &[u8; 32],
) {
    let outcome = Box::pin(wallet.pay_under_allowance(request, record))
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        unreachable!()
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let (_, ledger) = held(wallet, record).await;
    assert_eq!(ledger.unwrap().entries.len(), 1);
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

#[tokio::test]
async fn test_executor_floor_raised_before_the_paying_commit_holds_the_dispatch() {
    // Bob's request is dated above the floor the wallet holds. Right after the executor
    // reads the ledger, another payer app raises the floor past the request. The floor read
    // from the switch before the attempt's `Paying` commit holds the dispatch: nothing is
    // journaled `Paying` and nothing is sent (Paykit spec 8.1, "Blocked counterparties";
    // 8.3, "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let reads = Arc::new(ReadHook::new(Arc::clone(&w.router)));
    let (mut shop, mut wallet, record) = Box::pin(set_up_through(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
        Arc::clone(&reads) as Arc<dyn Transport>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let keeper = Box::pin(keeper_about_to_raise_the_floor(&w)).await;
    let ledger = ledger_key(&wallet, &record).await;
    let world = Arc::clone(&w);
    reads.after_read(
        &format!("/records/{}", pubky_switch_core::enc::b64(&ledger)),
        move || raise_the_floor(&world, keeper),
    );
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
    assert!(reads.fired());
    assert!(!wallet_store.paying_seen());
}

#[tokio::test]
async fn test_executor_floor_raised_after_the_paying_commit_holds_the_dispatch() {
    // While the attempt's `Paying` commit runs, another payer app raises the floor past
    // bob's request. The floor read from the switch again after the commit holds the
    // dispatch, and the attempt goes back to known unsubmitted (Paykit spec 8.1, "Blocked
    // counterparties"; 8.3, "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let keeper = Box::pin(keeper_about_to_raise_the_floor(&w)).await;
    let world = Arc::clone(&w);
    wallet_store.on_paying(move || raise_the_floor(&world, keeper));
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
    assert!(wallet_store.paying_seen());
}

#[tokio::test]
async fn test_executor_floor_read_failing_at_the_dispatch_holds_it() {
    // No raise, but the floor's read after the attempt's `Paying` commit fails: it never
    // counts as a floor that doesn't hold the request, so the dispatch is held (Paykit spec
    // 8.1, "Blocked counterparties"; 8.3, "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let world = Arc::clone(&w);
    wallet_store.on_paying(move || {
        // The request record is read again first, then the floor, which fails.
        world
            .alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: "/records/".into(),
                after: 1,
            });
    });
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
}

#[tokio::test]
async fn test_executor_payee_blocked_after_the_paying_commit_not_paid() {
    // While the attempt's `Paying` commit runs, another payer app blocks bob. The deny list
    // read from the switch again after the commit holds the dispatch: no automatic payment
    // to a blocked payee (Paykit spec 8.1, "Blocked counterparties"; 8.3, "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let mut keeper = Box::pin(keeper(&w)).await;
    let bob = w.bob_id();
    wallet_store.on_paying(move || {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async { keeper.app_mut().deny(&bob).await.unwrap() })
        })
        .join()
        .unwrap();
    });
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
}

/// Another app of alice's that holds the Allowance at `record` and may end it.
async fn own_app_holding(w: &World) -> Paykit {
    let mut other = Box::pin(executor_wallet(w, "other")).await;
    other.receive(None).await.unwrap();
    other
}

/// Run `other`'s end of the Allowance at `record` on its own thread, while the caller waits.
fn end_from(mut other: Paykit, record: [u8; 32]) {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { other.end_allowance(&record).await.unwrap() })
    })
    .join()
    .unwrap();
}

#[tokio::test]
async fn test_executor_allowance_ended_after_the_batch_holds_the_dispatch() {
    // Right after the executor's batch is applied, another own app ends the Allowance. The
    // copy the executor holds doesn't show the end; the Allowance record read from the
    // switch at the final payment check, before the attempt's `Paying` commit, does, and
    // holds the dispatch: nothing is journaled `Paying` and nothing is sent (Paykit spec
    // 8.3, "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let other = Box::pin(own_app_holding(&w)).await;
    wallet_store.on_attempted(move || end_from(other, record));
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
    assert!(!wallet_store.paying_seen());
}

#[tokio::test]
async fn test_executor_allowance_ended_after_the_paying_commit_holds_the_dispatch() {
    // While the attempt's `Paying` commit runs, another own app ends the Allowance. The
    // Allowance record read from the switch again after the commit holds the dispatch, and
    // the attempt goes back to known unsubmitted (Paykit spec 8.3, "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let other = Box::pin(own_app_holding(&w)).await;
    wallet_store.on_paying(move || end_from(other, record));
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
    assert!(wallet_store.paying_seen());
}

#[tokio::test]
async fn test_executor_repeat_held_under_an_allowance_ended_before_any_call() {
    // While the attempt's `Paying` commit runs, another own app ends the Allowance. The
    // final payment check holds the dispatch, and the process stops before the attempt is
    // written back to `Attempted`, so `Paying` stays journaled with nothing dispatched. The
    // resume's repeat on the idempotent method reads the Allowance record from the switch,
    // finds it ended and holds: the method is never called, and the attempt stays reserved
    // in the ledger (Paykit spec 7.2, 8.3, "Executor").
    let w = Arc::new(World::new());
    w.rail.set_declaration(
        paykit_sdk::rail::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let other = Box::pin(own_app_holding(&w)).await;
    wallet_store.on_paying(move || end_from(other, record));
    wallet_store.fail_the_write_back();
    assert!(Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .is_err());
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = Paykit::load(
        w.env(&w.alice, Arc::clone(&wallet_store) as Arc<dyn StateStore>),
        paykit_sdk::PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Paying
    );
    w.clock.advance_ms(MINUTE_MS);
    Box::pin(wallet.resume_payments()).await.unwrap();
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Paying
    );
    let (_, ledger) = held(&wallet, &record).await;
    assert_eq!(ledger.unwrap().entries.len(), 1);
}

#[tokio::test]
async fn test_executor_allowance_read_failing_at_the_dispatch_holds_it() {
    // No end, but the Allowance record's read after the attempt's `Paying` commit fails: it
    // never counts as an Allowance still in force, so the dispatch is held (Paykit spec 8.3,
    // "Executor").
    let w = Arc::new(World::new());
    let wallet_store = Arc::new(PayingStore::default());
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(
        &w,
        Arc::clone(&wallet_store) as Arc<dyn StateStore>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let world = Arc::clone(&w);
    wallet_store.on_paying(move || {
        world
            .alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: format!("/records/{}", pubky_switch_core::enc::b64(&record)),
                after: 0,
            });
    });
    Box::pin(held_at_the_dispatch(&w, &mut wallet, &request, &record)).await;
    assert!(wallet_store.paying_seen());
}

#[tokio::test]
async fn test_executor_pays_nothing_while_the_client_stores_an_answer_of_its_counterparty() {
    // An acceptance from bob comes while bob's standing can't be checked: the client stores
    // it unverified and tries it again at each read. It may be an earlier answer than the
    // record's response, so while it is stored the executor pays nothing under an Allowance
    // of bob's; once a read takes it, the payment goes (Paykit spec 8.1, "Stored entries";
    // 8.3, "Conflicted").
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let mut unexpiring = draft("0.5", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = Box::pin(request_from(&mut shop, w.alice_id(), unexpiring)).await;
    Box::pin(wallet.receive(None)).await.unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.allowance_acceptance",
        vec![[7; 32]],
        json!({ "allowance_id": paykit_lib::AllowanceId::generate().unwrap().as_str() })
            .as_object()
            .cloned()
            .unwrap(),
    );
    w.homeserver.set_down(true);
    // Past the 10 minutes bob's `identity.json` is cached for, and the bounded wait.
    w.clock.advance_ms(11 * 60_000);
    let _ = Box::pin(wallet.receive(None)).await;
    w.clock
        .advance_ms(pubky_switch_client::app::DEFAULT_PARK_AFTER_MS + 1);
    let _ = Box::pin(wallet.receive(None)).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    w.homeserver.set_down(false);
    let refused = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "stores an entry"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    // The next read verifies bob and takes the entry: nothing is stored, and the request is
    // paid under the Allowance.
    Box::pin(wallet.receive(None)).await.unwrap();
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let paid = Box::pin(wallet.pay_under_allowance(&request, &record))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_executor_attempt_rebuilt_after_a_restore_keeps_its_allowance() {
    // The backup is taken after the grant; the payment under the Allowance is made after it
    // and left in flight, then the device is lost. The restored app's journal doesn't hold
    // the attempt, so its reconcile rebuilds the entry from the request record, and the
    // ledger it holds has an entry for the attempt: the proof names the Allowance, as the
    // executor's own would (Paykit spec 8.3 step 4).
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let id = wallet.allowance(&record).await.unwrap().allowance_id;
    let backup = wallet.backup().await.unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(wallet.pay_under_allowance(&request, &record))
            .await
            .unwrap()
    else {
        panic!("in flight");
    };
    w.rail.settle(&attempt_id, true);
    drop(wallet);

    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    let report = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
        "{report:?}"
    );
    Box::pin(wallet.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(wallet.receive(None)).await.unwrap();
    assert_eq!(w.rail.executions(&attempt_id), 1);

    Box::pin(shop.receive(None)).await.unwrap();
    let mut proofs = 0;
    for held in shop.app().records_of("payee_activity").await.unwrap() {
        let value: Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
        let Some(proof) = value["proof"]["plaintext"]["json"].as_str() else {
            continue;
        };
        let bytes = pubky_switch_core::enc::unb64(proof).unwrap();
        let proof: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(proof["body"]["allowance_id"], json!(id.as_str()));
        proofs += 1;
    }
    assert_eq!(proofs, 1);
}

/// `backup` with its kept ledger version's content padded, by a member the SDK doesn't
/// know, to `len` bytes.
fn with_kept_ledger_of_len(backup: &Value, len: usize) -> Value {
    let mut state = crate::restore::layer_of(backup);
    let (_, kept) = state
        .iter_mut()
        .find(|(key, _)| key.starts_with(b"lv/"))
        .unwrap();
    let bytes = pubky_switch_core::enc::unb64(kept["d"]["value"].as_str().unwrap()).unwrap();
    let mut ledger: Value = serde_json::from_slice(&bytes).unwrap();
    ledger["pad"] = json!("");
    let unpadded = serde_json::to_vec(&ledger).unwrap().len();
    ledger["pad"] = json!("a".repeat(len - unpadded));
    let padded = serde_json::to_vec(&ledger).unwrap();
    assert_eq!(padded.len(), len);
    Ledger::from_json(&padded).unwrap();
    kept["d"]["value"] = json!(pubky_switch_core::enc::b64(&padded));
    crate::restore::with_layer(backup, state)
}

#[tokio::test]
async fn test_executor_kept_ledger_past_the_record_size_refused_at_restore() {
    // A kept ledger version is a shared record value, at most 256 KiB (Switch wire 6.2): no
    // run keeps a longer one, so a backup holding one is refused, nothing written; one at
    // the limit is restored.
    const MAX_RECORD_VALUE: usize = 256 * 1024;
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let first = request(&w, &mut shop, &mut wallet, "0.5").await;
    wallet.pay_under_allowance(&first, &record).await.unwrap();
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    let good: Value = serde_json::from_slice(&bytes).unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let over = with_kept_ledger_of_len(&good, MAX_RECORD_VALUE + 1);
    crate::restore::refused_writing_nothing(&w, over, app_id, "lv/").await;
    let at = with_kept_ledger_of_len(&good, MAX_RECORD_VALUE);
    let backup = paykit_sdk::PaykitBackup::from_bytes(&serde_json::to_vec(&at).unwrap()).unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app().protocol_scan(b"lv/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_executor_refuses_an_occurrence_attempted_by_hand() {
    // The user pays bob's request by hand and the payment method refuses the call: the
    // attempt is written `failed`, and no money moved. The attempt's batch marked the record
    // `manual_only`, so its executor never pays it: it is paid again only by hand (Paykit
    // spec 8.3, "Manual only").
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let by_hand = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(by_hand, PayOutcome::Failed { .. }), "{by_hand:?}");
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "manual-only"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    let (_, ledger) = held(&wallet, &record).await;
    assert!(ledger.unwrap().entries.is_empty());
    // By hand it is still paid.
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
}

#[tokio::test]
async fn test_executor_pays_after_its_own_failed_attempt_is_compacted() {
    // The executor's own attempt fails, and its ledger entry is compacted into
    // `totals.failed` a minute later. The ledger still names the attempt, so it is the
    // Allowance's own, and the executor pays the occurrence again (Paykit spec 8.3,
    // "Doubtful obligations"; Paykit wire 5.5).
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let failed = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(failed, PayOutcome::Failed { .. }), "{failed:?}");
    let (allowance, ledger) = held(&wallet, &record).await;
    let mut ledger = ledger.unwrap();
    let entry = ledger.entries.remove(0);
    assert!(ledger.entries.is_empty());
    ledger.failed.push(paykit_lib::allowances::CompactedFailed {
        obligation: entry.obligation,
        attempt_id: entry.attempt_id,
        value: entry.value,
    });
    w.clock
        .advance_ms(paykit_lib::allowances::ENTRY_WINDOW_MS + 1_000);
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    let version = wallet
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .version;
    let op = wallet
        .app_mut()
        .record_op(
            &target,
            Some(&ledger.to_json().unwrap()),
            vec![pubky_switch_core::ops::Condition::RecordVersion {
                key: record,
                version,
            }],
        )
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(op))
        .await
        .unwrap();
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let (_, ledger) = held(&wallet, &record).await;
    let ledger = ledger.unwrap();
    assert_eq!(ledger.failed.len(), 1);
    assert_eq!(ledger.entries.len(), 1);
}

#[tokio::test]
async fn test_executor_refuses_an_occurrence_attempted_under_another_allowance() {
    // Bob allows alice two Allowances, each with its own executor. The first executor's
    // attempt at bob's request fails. The second executor still holds the request record as
    // it was before, naming no Allowance; the claim it reads shows the other Allowance's
    // attempt, which its own ledger doesn't name, so it pays nothing (Paykit spec 8.3,
    // "Doubtful obligations").
    let w = World::new();
    let mut shop = allowance_shop(&w).await;
    let mut wallet = executor_wallet(&w, "wallet").await;
    let first = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    let second = accepted(&w, &mut shop, &mut wallet, terms_with(json!({}))).await;
    wallet
        .grant_executor(&first, UserConfirmed::new())
        .await
        .unwrap();
    let mut other = executor_wallet(&w, "wallet2").await;
    other.receive(None).await.unwrap();
    other
        .grant_executor(&second, UserConfirmed::new())
        .await
        .unwrap();
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    other.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let failed = wallet.pay_under_allowance(&request, &first).await.unwrap();
    assert!(matches!(failed, PayOutcome::Failed { .. }), "{failed:?}");
    let refused = other
        .pay_under_allowance(&request, &second)
        .await
        .unwrap_err();
    assert!(
        refused_for(&refused, "attempted by hand or under another Allowance"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let (_, ledger) = held(&other, &second).await;
    assert!(ledger.is_none_or(|l| l.entries.is_empty()));
}

/// The payer record of `request` as `app` holds it, as JSON.
async fn request_record_value(app: &Paykit, request: &RequestRef) -> Value {
    let target = crate::outcome_record::payer_record(request);
    let key = app.app().record_key(&target).unwrap();
    let held = app.app().shared_record(&key).await.unwrap().unwrap();
    serde_json::from_slice(held.content.value().unwrap()).unwrap()
}

/// The claim of the occurrence `app`'s one journaled payment is of, read from the switch.
async fn claim_of(app: &mut Paykit) -> Option<pubky_switch_client::app::ClaimRead> {
    let obligation = app.payments().await.unwrap()[0].obligation;
    app.app_mut().read_claim(&obligation).await.unwrap()
}

/// An Allowance with the wallet as its executor, and alice's phone, another app of hers,
/// that pays bob's request by hand, the payment method answering `behaviour`: it fails, or
/// refuses the call, so nothing is submitted. The wallet has read its messages since.
async fn attempted_by_the_phone(
    w: &World,
    behaviour: paykit_sdk::rail::Behaviour,
) -> (Paykit, Paykit, [u8; 32], RequestRef) {
    let (mut shop, mut wallet, record) = set_up(w, terms_with(json!({}))).await;
    let mut phone = Box::pin(executor_wallet(w, "phone")).await;
    let request = request(w, &mut shop, &mut wallet, "0.5").await;
    phone.receive(None).await.unwrap();
    w.rail.next(behaviour);
    let by_hand = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(by_hand, PayOutcome::Failed { .. }), "{by_hand:?}");
    wallet.receive(None).await.unwrap();
    (phone, wallet, record, request)
}

/// The executor's payment of `request`, refused for the record's `manual_only` mark, with
/// nothing written and nothing paid.
async fn refused_as_manual_only(
    w: &World,
    wallet: &mut Paykit,
    request: &RequestRef,
    record: &[u8; 32],
) {
    let refused = wallet
        .pay_under_allowance(request, record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "manual-only"), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    let (_, ledger) = held(wallet, record).await;
    assert!(ledger.unwrap().entries.is_empty());
}

#[tokio::test]
async fn test_executor_manual_only_after_another_apps_failed_attempt_by_hand() {
    // Alice's phone pays bob's request by hand and the payment fails. Its attempt's batch
    // marked the request record `manual_only`: the wallet, the Allowance's executor, doesn't
    // pay it (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let (_phone, mut wallet, record, request) = Box::pin(attempted_by_the_phone(
        &w,
        paykit_sdk::rail::Behaviour::Fail,
    ))
    .await;
    assert!(wallet.request(&request).await.unwrap().manual_only);
    refused_as_manual_only(&w, &mut wallet, &request, &record).await;
}

#[tokio::test]
async fn test_executor_manual_only_after_another_apps_attempt_by_hand_never_submitted() {
    // Alice's phone pays bob's request by hand and the payment method refuses the call:
    // nothing was submitted. The mark its attempt's batch wrote still holds: the executor
    // doesn't pay it (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let (_phone, mut wallet, record, request) = Box::pin(attempted_by_the_phone(
        &w,
        paykit_sdk::rail::Behaviour::Refuse,
    ))
    .await;
    refused_as_manual_only(&w, &mut wallet, &request, &record).await;
}

#[tokio::test]
async fn test_executor_manual_only_after_an_attempt_by_hand_through_a_restore() {
    // The wallet is backed up before alice's phone pays bob's request by hand, and restored
    // after: the backup holds the record without the mark, the switch with it. The restored
    // executor still doesn't pay it (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let (mut shop, wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let mut wallet = wallet;
    let mut phone = Box::pin(executor_wallet(&w, "phone")).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    phone.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let by_hand = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(by_hand, PayOutcome::Failed { .. }), "{by_hand:?}");
    let mut wallet = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    assert!(wallet.request(&request).await.unwrap().manual_only);
    refused_as_manual_only(&w, &mut wallet, &request, &record).await;
}

#[tokio::test]
async fn test_executor_hand_batch_in_conflict_writes_no_attempt_and_no_mark() {
    // Alice's phone writes the request record after the wallet read it. The wallet's payment
    // by hand builds on the version it read: the switch refuses the batch, and neither the
    // attempt nor the mark is written. Paid by hand again once the wallet has read the
    // record, the attempt and the mark land together (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let (mut shop, mut wallet, _record) = set_up(&w, terms_with(json!({}))).await;
    let mut phone = Box::pin(executor_wallet(&w, "phone")).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    phone.receive(None).await.unwrap();
    let value = serde_json::to_vec(&request_record_value(&phone, &request).await).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&value)).await;
    let conflict = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(conflict, PaykitError::Conflict(_)), "{conflict:?}");
    assert!(w.rail.all_executions().is_empty());
    let claim = claim_of(&mut wallet).await;
    assert!(claim.is_none_or(|read| read.view.attempts.is_empty()));
    phone
        .app_mut()
        .read_record(&crate::outcome_record::payer_record(&request))
        .await
        .unwrap();
    assert!(request_record_value(&phone, &request)
        .await
        .get("manual_only")
        .is_none());
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(wallet.request(&request).await.unwrap().manual_only);
    let claim = claim_of(&mut wallet).await.unwrap();
    assert_eq!(claim.view.attempts.len(), 1);
}

#[tokio::test]
async fn test_executor_automatic_payment_writes_no_mark() {
    // An executor's payment, and an automatic one made without an Allowance, leave the
    // request record without `manual_only`.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let under = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet.pay_under_allowance(&under, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(!wallet.request(&under).await.unwrap().manual_only);
    assert!(request_record_value(&wallet, &under)
        .await
        .get("manual_only")
        .is_none());
    let automatic = request(&w, &mut shop, &mut wallet, "0.5").await;
    let paid = wallet
        .pay_request_automatically(&automatic, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(!wallet.request(&automatic).await.unwrap().manual_only);
}

#[tokio::test]
async fn test_executor_mark_by_hand_keeps_the_records_other_members() {
    // The executor's attempt at bob's request fails: the record names the Allowance and
    // shows the outcome. The user then pays it by hand, left in flight. The record the
    // attempt's batch wrote is the one read, with `manual_only` set and nothing else
    // changed.
    let w = World::new();
    let (mut shop, mut wallet, record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let failed = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(failed, PayOutcome::Failed { .. }), "{failed:?}");
    let mut before = request_record_value(&wallet, &request).await;
    assert!(before["allowance_id"].is_string(), "{before}");
    assert!(before.get("manual_only").is_none(), "{before}");
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let pending = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    before["manual_only"] = json!(true);
    assert_eq!(request_record_value(&wallet, &request).await, before);
}

/// Alice's wallet pays bob's request by hand on an idempotent method, the call left in
/// flight after `Paying` is journaled, as a crash would leave it.
async fn paid_by_hand_in_flight(w: &World) -> (Paykit, RequestRef, paykit_lib::AttemptId) {
    w.rail.set_declaration(
        paykit_sdk::rail::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut shop = allowance_shop(w).await;
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().contacts().build().unwrap(),
            crate::common::day_window(),
            crate::common::store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let request = request(w, &mut shop, &mut wallet, "0.5").await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    (wallet, request, attempt_id)
}

#[tokio::test]
async fn test_executor_mark_by_hand_doesnt_withhold_that_payments_repeat() {
    // The user's payment by hand marked the record `manual_only` in its attempt's batch, and
    // its call was left in flight. At the resume, the repeat finishes the user's own payment:
    // the mark doesn't withhold it, and once the method settles it the payment completes
    // (Paykit spec 7.2; 8.3, "Manual only").
    let w = World::new();
    let (mut wallet, request, attempt) = Box::pin(paid_by_hand_in_flight(&w)).await;
    assert!(wallet.request(&request).await.unwrap().manual_only);
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Paying);
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 2);
    w.rail.settle(&attempt, true);
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
}

#[tokio::test]
async fn test_executor_repeat_withheld_over_a_mark_written_since() {
    // The executor's call is left in flight; then the user marks the request `manual_only`.
    // At the resume, the repeat is of an automatic payment: the mark withholds it, and the
    // attempt stays open for the method's history or a receipt (Paykit spec 8.3).
    let w = World::new();
    let (_shop, mut wallet, _record, attempt, request) =
        Box::pin(executor_in_flight_on(&w, true)).await;
    wallet.mark_manual_only(&request).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Paying);
}

#[tokio::test]
async fn test_executor_entry_rebuilt_after_a_restore_kept_as_automatic() {
    // The user pays bob's request by hand after the backup and the device is lost once the
    // method has paid it. The restored app's reconcile rebuilds the entry from the claim and
    // the request record, which don't tell who started the attempt: it is kept as automatic,
    // so what holds an automatic attempt, the mark the payment's batch wrote included, holds
    // it. A rebuilt entry is never dispatched again anyway; its outcome is the method's.
    let w = World::new();
    let (mut shop, mut wallet, _record) = set_up(&w, terms_with(json!({}))).await;
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let backup = wallet.backup().await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    w.rail.settle(&attempt_id, true);
    drop(wallet);
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    let report = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
        "{report:?}"
    );
    let journal = wallet.app().protocol_scan(b"j/").await.unwrap();
    assert_eq!(journal.len(), 1);
    let kept: Value = serde_json::from_slice(&journal[0].1).unwrap();
    assert_eq!(kept["d"]["initiator"], "automatic", "{kept}");
    assert_eq!(kept["d"]["restored"], true, "{kept}");
    assert!(wallet.request(&request).await.unwrap().manual_only);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[derive(Clone, Copy)]
enum RestoredLedger {
    Unavailable,
    WrongWriter,
    Unconditioned,
    Divergent,
    WrongObligation,
    WrongAmount,
    WrongAsset,
}

async fn restore_with_uncertain_ledger(case: RestoredLedger) {
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let allowance_id = wallet.allowance(&record).await.unwrap().allowance_id;
    let backup = wallet.backup().await.unwrap();
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(wallet.pay_under_allowance(&request, &record))
            .await
            .unwrap()
    else {
        panic!("in flight")
    };
    w.rail.settle(&attempt_id, true);
    let (allowance, ledger) = held(&wallet, &record).await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "allowance_ledgers",
            paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::allowance(
            &allowance.counterparty,
            &allowance.allowance_id,
        ),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let version = wallet
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .version;
    let mut ledger = ledger.unwrap();
    match case {
        RestoredLedger::WrongObligation => ledger.entries[0].obligation = [0x71; 32],
        RestoredLedger::WrongAmount => {
            ledger.entries[0].value = paykit_lib::Decimal::parse("0.7").unwrap()
        }
        RestoredLedger::WrongAsset => {
            ledger.entries[0].asset = paykit_lib::Asset::parse("eur").unwrap()
        }
        _ => {}
    }
    let conditions = vec![pubky_switch_core::ops::Condition::RecordVersion {
        key: record,
        version,
    }];
    match case {
        RestoredLedger::WrongWriter => {
            let mut other = Box::pin(executor_wallet(&w, "other")).await;
            other.app_mut().read_record(&target).await.unwrap();
            other
                .app_mut()
                .put_record(&target, &ledger.to_json().unwrap(), conditions)
                .await
                .unwrap();
        }
        RestoredLedger::Unconditioned => {
            wallet
                .app_mut()
                .put_record(&target, &ledger.to_json().unwrap(), Vec::new())
                .await
                .unwrap();
        }
        RestoredLedger::WrongObligation
        | RestoredLedger::WrongAmount
        | RestoredLedger::WrongAsset => {
            wallet
                .app_mut()
                .put_record(&target, &ledger.to_json().unwrap(), conditions)
                .await
                .unwrap();
        }
        RestoredLedger::Divergent => {
            w.alice
                .switch()
                .make_divergent(UPLOADER, key, [1; 32])
                .unwrap();
        }
        RestoredLedger::Unavailable => {}
    }
    drop(wallet);
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    if matches!(case, RestoredLedger::Unavailable) {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: format!("/records/{}", pubky_switch_core::enc::b64(&key)),
                after: 0,
            });
    }
    let report = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(report.resolved.is_empty(), "{report:?}");
    assert!(
        report
            .doubtful
            .iter()
            .any(|d| d.attempts.contains(&attempt_id)),
        "{report:?}"
    );
    assert!(wallet.payments().await.unwrap().is_empty());
    assert_eq!(w.rail.executions(&attempt_id), 1);
    if matches!(case, RestoredLedger::Unavailable) {
        let report = Box::pin(wallet.reconcile()).await.unwrap();
        assert!(
            matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
            "{report:?}"
        );
        let journal = wallet.app().protocol_scan(b"j/").await.unwrap();
        let entry: Value = serde_json::from_slice(&journal[0].1).unwrap();
        assert_eq!(entry["d"]["allowance_id"], allowance_id.as_str());
        assert_eq!(w.rail.executions(&attempt_id), 1);
    }
}

#[tokio::test]
async fn test_executor_restore_unavailable_ledger_retries_attribution() {
    Box::pin(restore_with_uncertain_ledger(RestoredLedger::Unavailable)).await;
}

#[tokio::test]
async fn test_executor_restore_wrong_ledger_writer_keeps_attempt_doubtful() {
    Box::pin(restore_with_uncertain_ledger(RestoredLedger::WrongWriter)).await;
}

#[tokio::test]
async fn test_executor_restore_unconditioned_ledger_keeps_attempt_doubtful() {
    Box::pin(restore_with_uncertain_ledger(RestoredLedger::Unconditioned)).await;
}

#[tokio::test]
async fn test_executor_restore_divergent_ledger_keeps_attempt_doubtful() {
    Box::pin(restore_with_uncertain_ledger(RestoredLedger::Divergent)).await;
}

#[tokio::test]
async fn test_executor_restore_wrong_ledger_obligation_keeps_attempt_doubtful() {
    Box::pin(restore_with_uncertain_ledger(
        RestoredLedger::WrongObligation,
    ))
    .await;
}

#[tokio::test]
async fn test_executor_restore_wrong_ledger_amount_keeps_attempt_doubtful() {
    Box::pin(restore_with_uncertain_ledger(RestoredLedger::WrongAmount)).await;
}

#[tokio::test]
async fn test_executor_restore_wrong_ledger_asset_keeps_attempt_doubtful() {
    Box::pin(restore_with_uncertain_ledger(RestoredLedger::WrongAsset)).await;
}

async fn restored_destination_uses_terms(from_list: bool) {
    use crate::common::endpoint;
    use paykit_lib::{
        bodies::{ListEndpoint, RequestDestination},
        PaymentEndpointPayload,
    };
    let w = World::new();
    let allowed = "btc-lightning-bolt11";
    let excluded = "btc-bitcoin-p2wpkh";
    let (mut shop, mut wallet, record) = Box::pin(set_up(
        &w,
        terms_with(json!({
            "allowed_payment_endpoint_identifiers": [allowed]
        })),
    ))
    .await;
    let mut draft = draft("0.5", w.clock.now_ms());
    if from_list {
        let endpoints = [
            (
                endpoint(allowed),
                ListEndpoint {
                    payload: PaymentEndpointPayload::new("lnbc1single").unwrap(),
                    single_use: true,
                },
            ),
            (
                endpoint(excluded),
                ListEndpoint {
                    payload: PaymentEndpointPayload::new("bc1qsingle").unwrap(),
                    single_use: true,
                },
            ),
        ]
        .into_iter()
        .collect();
        w.list_to_alice_with(&mut shop, &[wallet.app_id()], w.clock.now_ms(), endpoints)
            .await;
        wallet.receive(None).await.unwrap();
        draft.destination = RequestDestination::Any;
    } else if let RequestDestination::Endpoints(endpoints) = &mut draft.destination {
        endpoints.insert(
            endpoint(excluded),
            PaymentEndpointPayload::new("bc1qrequest").unwrap(),
        );
    }
    let request = Box::pin(request_from(&mut shop, w.alice_id(), draft)).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(wallet.pay_under_allowance(&request, &record))
            .await
            .unwrap()
    else {
        panic!("in flight")
    };
    w.rail.settle(&attempt_id, true);
    if from_list {
        let target = pubky_switch_client::app::RecordTarget::new(
            pubky_switch_client::app::Collection::new(
                "payment_lists",
                paykit_lib::collections::Collection::PaymentLists.fixed_policy(),
            ),
            &paykit_lib::derive::natural_key::payment_list(&w.bob_id(), &shop.app_id()),
        );
        let key = wallet.app().record_key(&target).unwrap();
        let shared = wallet.app().shared_record(&key).await.unwrap().unwrap();
        let mut value: Value = serde_json::from_slice(shared.content.value().unwrap()).unwrap();
        let mut second = value["used"][0].clone();
        second["endpoint"] = json!(excluded);
        value["used"].as_array_mut().unwrap().push(second);
        wallet
            .app_mut()
            .put_record(&target, &serde_json::to_vec(&value).unwrap(), Vec::new())
            .await
            .unwrap();
    }
    drop(wallet);
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    let report = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
        "{report:?}"
    );
    let journal = wallet.app().protocol_scan(b"j/").await.unwrap();
    let entry: Value = serde_json::from_slice(&journal[0].1).unwrap();
    assert_eq!(entry["d"]["destination"]["endpoint_id"], allowed);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_executor_restore_request_destination_uses_allowance_identifiers() {
    Box::pin(restored_destination_uses_terms(false)).await;
}

#[tokio::test]
async fn test_executor_restore_marked_list_destination_uses_allowance_identifiers() {
    Box::pin(restored_destination_uses_terms(true)).await;
}

async fn removed_write_at_cutoff(ledger: bool, after: bool) {
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    other.receive(None).await.unwrap();
    let key = if ledger {
        let first = Box::pin(request(&w, &mut shop, &mut wallet, "0.1")).await;
        assert!(matches!(
            Box::pin(wallet.pay_under_allowance(&first, &record))
                .await
                .unwrap(),
            PayOutcome::Paid { .. }
        ));
        other.receive(None).await.unwrap();
        other
            .grant_executor(&record, UserConfirmed::new())
            .await
            .unwrap();
        std::mem::swap(&mut wallet, &mut other);
        ledger_key(&wallet, &record).await
    } else {
        let (allowance, _) = held(&other, &record).await;
        other
            .app_mut()
            .put_record(
                &allowances_target(&allowance),
                &allowance.to_json().unwrap(),
                Vec::new(),
            )
            .await
            .unwrap();
        wallet.receive(None).await.unwrap();
        record
    };
    let shared = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(shared.writer, other.app_id());
    let cutoff = shared.written_at_ms.unwrap() - u64::from(after);
    let other_id = other.app_id();
    w.alice
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != other_id);
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: other_id,
                distrust_from: cutoff,
            });
        })
        .unwrap();
    w.clock.advance_ms(1_000);
    wallet.receive(None).await.unwrap();
    let next = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let result = Box::pin(wallet.pay_under_allowance(&next, &record)).await;
    if after {
        assert!(
            result.as_ref().err().is_some_and(not_automatic),
            "{result:?}"
        );
    } else {
        assert!(matches!(result, Ok(PayOutcome::Paid { .. })), "{result:?}");
    }
}

#[tokio::test]
async fn test_executor_allowance_write_at_removal_cutoff_keeps_standing() {
    Box::pin(removed_write_at_cutoff(false, false)).await;
}

#[tokio::test]
async fn test_executor_allowance_write_after_removal_cutoff_is_refused() {
    Box::pin(removed_write_at_cutoff(false, true)).await;
}

#[tokio::test]
async fn test_executor_ledger_write_at_removal_cutoff_keeps_standing() {
    Box::pin(removed_write_at_cutoff(true, false)).await;
}

#[tokio::test]
async fn test_executor_ledger_write_after_removal_cutoff_is_refused() {
    Box::pin(removed_write_at_cutoff(true, true)).await;
}

async fn restored_manual_attempt_is_not_attributed(forged: bool) {
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    assert!(matches!(
        Box::pin(wallet.pay_under_allowance(&request, &record))
            .await
            .unwrap(),
        PayOutcome::Failed { .. }
    ));
    let backup = wallet.backup().await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(wallet.pay_request(&request, PayOptions::default()))
            .await
            .unwrap()
    else {
        panic!("in flight")
    };
    w.rail.settle(&attempt_id, true);
    if forged {
        let (allowance, ledger) = held(&wallet, &record).await;
        let mut ledger = ledger.unwrap();
        let mut entry = ledger.entries[0].clone();
        entry.attempt_id = attempt_id;
        ledger.entries.push(entry);
        let target = pubky_switch_client::app::RecordTarget::new(
            pubky_switch_client::app::Collection::new(
                "allowance_ledgers",
                paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
            ),
            &paykit_lib::derive::natural_key::allowance(
                &allowance.counterparty,
                &allowance.allowance_id,
            ),
        );
        let mut other = Box::pin(executor_wallet(&w, "other")).await;
        other.receive(None).await.unwrap();
        let version = other
            .app()
            .shared_record(&record)
            .await
            .unwrap()
            .unwrap()
            .version;
        other
            .app_mut()
            .put_record(
                &target,
                &ledger.to_json().unwrap(),
                vec![pubky_switch_core::ops::Condition::RecordVersion {
                    key: record,
                    version,
                }],
            )
            .await
            .unwrap();
    }
    drop(wallet);
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    let report = Box::pin(wallet.reconcile()).await.unwrap();
    if forged {
        assert!(report.resolved.is_empty(), "{report:?}");
        assert!(
            report
                .doubtful
                .iter()
                .any(|d| d.attempts.contains(&attempt_id)),
            "{report:?}"
        );
    } else {
        assert!(
            matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
            "{report:?}"
        );
        let journal = wallet.app().protocol_scan(b"j/").await.unwrap();
        let entry: Value = journal
            .iter()
            .map(|(_, value)| serde_json::from_slice::<Value>(value).unwrap())
            .find(|value| value["d"]["attempt_id"] == attempt_id.to_text())
            .unwrap();
        assert!(entry["d"]["allowance_id"].is_null(), "{entry}");
    }
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_executor_restore_manual_attempt_uses_validated_ledger_absence() {
    Box::pin(restored_manual_attempt_is_not_attributed(false)).await;
}

#[tokio::test]
async fn test_executor_restore_unauthorized_ledger_cannot_attribute_manual_attempt() {
    Box::pin(restored_manual_attempt_is_not_attributed(true)).await;
}

#[tokio::test]
async fn test_executor_restore_retains_allowance_divergence_until_confirmation() {
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    let backup = wallet.backup().await.unwrap();
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(wallet.pay_under_allowance(&request, &record))
            .await
            .unwrap()
    else {
        panic!("in flight")
    };
    w.rail.settle(&attempt_id, true);
    drop(wallet);
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    w.alice
        .switch()
        .make_divergent(UPLOADER, record, [1; 32])
        .unwrap();
    let first = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(first.resolved.is_empty(), "{first:?}");
    assert!(
        first
            .doubtful
            .iter()
            .any(|d| d.attempts.contains(&attempt_id)),
        "{first:?}"
    );
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &allowance.to_json().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let second = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(
        second.resolved.is_empty(),
        "resolved after an unconfirmed divergent view: {second:?}"
    );
    assert!(
        second
            .doubtful
            .iter()
            .any(|d| d.attempts.contains(&attempt_id)),
        "{second:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    let confirmed = Box::pin(wallet.reconcile()).await.unwrap();
    assert!(
        matches!(confirmed.resolved.as_slice(), [PayOutcome::Paid { .. }]),
        "{confirmed:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

async fn payment_keeps_observed_allowance_pause(at_grant: bool) {
    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    w.alice
        .switch()
        .make_divergent(UPLOADER, record, [1; 32])
        .unwrap();
    if at_grant {
        let first = Box::pin(wallet.grant_executor(&record, UserConfirmed::new())).await;
        assert!(
            matches!(
                first.as_ref().err().and_then(PaykitError::refusal),
                Some(Refusal::NotNow(_))
            ),
            "{first:?}"
        );
    } else {
        let first = Box::pin(wallet.pay_under_allowance(&request, &record)).await;
        assert!(first.as_ref().err().is_some_and(not_automatic), "{first:?}");
    }
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &allowance.to_json().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let second = Box::pin(wallet.pay_under_allowance(&request, &record)).await;
    assert!(
        second.as_ref().err().is_some_and(not_automatic),
        "paid after an unconfirmed divergent view: {second:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    let confirmed = Box::pin(wallet.pay_under_allowance(&request, &record)).await;
    assert!(
        matches!(confirmed, Ok(PayOutcome::Paid { .. })),
        "{confirmed:?}"
    );
}

#[tokio::test]
async fn test_executor_direct_payment_retains_allowance_divergence_until_confirmation() {
    Box::pin(payment_keeps_observed_allowance_pause(false)).await;
}

#[tokio::test]
async fn test_executor_grant_retains_allowance_divergence_until_confirmation() {
    Box::pin(payment_keeps_observed_allowance_pause(true)).await;
}

#[tokio::test]
async fn test_executor_repeat_retains_allowance_divergence_until_confirmation() {
    let w = World::new();
    let (_shop, mut wallet, record, attempt) = Box::pin(executor_in_flight(&w)).await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    w.alice
        .switch()
        .make_divergent(UPLOADER, record, [1; 32])
        .unwrap();
    w.clock.advance_ms(60_000);
    Box::pin(wallet.resume_payments()).await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt), 1);
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &allowance.to_json().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    Box::pin(wallet.resume_payments()).await.unwrap();
    assert_eq!(
        w.rail.pay_calls(&attempt),
        1,
        "repeated after an unconfirmed divergent view"
    );
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    Box::pin(wallet.resume_payments()).await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt), 2);
}

#[derive(Default)]
pub(crate) struct FailPauseStore {
    inner: pubky_switch_client::store::MemoryStore,
    armed: std::sync::atomic::AtomicBool,
    fired: std::sync::atomic::AtomicBool,
}

impl FailPauseStore {
    pub(crate) fn arm(&self) {
        self.fired.store(false, std::sync::atomic::Ordering::SeqCst);
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn failed(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl StateStore for FailPauseStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        self.inner.get(ns, key)
    }
    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }
    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        let pause = batch.writes().iter().any(|write| matches!(write, pubky_switch_client::store::Write::Put { key, .. } if key.starts_with(b"p/dv/")));
        if pause && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseAction {
    Direct,
    Grant,
    Repeat,
    Restore,
}

async fn failed_pause_call(
    wallet: &mut Paykit,
    request: &RequestRef,
    record: &[u8; 32],
    action: PauseAction,
) -> Result<(), PaykitError> {
    match action {
        PauseAction::Direct => Box::pin(wallet.pay_under_allowance(request, record))
            .await
            .map(|_| ()),
        PauseAction::Grant => Box::pin(wallet.grant_executor(record, UserConfirmed::new()))
            .await
            .map(|_| ()),
        PauseAction::Repeat => Box::pin(wallet.resume_payments()).await.map(|_| ()),
        PauseAction::Restore => Box::pin(wallet.reconcile()).await.map(|_| ()),
    }
}

async fn failed_pause_retry(action: PauseAction, reload: bool, retries: usize) {
    let w = World::new();
    if action != PauseAction::Restore {
        w.rail.set_declaration(
            paykit_sdk::rail::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
                .with_idempotent(true),
        );
    }
    let failing = Arc::new(FailPauseStore::default());
    let base = Arc::clone(&failing) as Arc<dyn StateStore>;
    let (mut shop, mut wallet, record) = Box::pin(set_up_on(&w, Arc::clone(&base))).await;
    let mut other = Box::pin(executor_wallet(&w, "other")).await;
    let backup = wallet.backup().await.unwrap();
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    let attempt = if matches!(action, PauseAction::Repeat | PauseAction::Restore) {
        w.rail.next(paykit_sdk::rail::Behaviour::Hang);
        let PayOutcome::Pending { attempt_id, .. } =
            Box::pin(wallet.pay_under_allowance(&request, &record))
                .await
                .unwrap()
        else {
            panic!("in flight")
        };
        Some(attempt_id)
    } else {
        None
    };
    let (failing, base) = if action == PauseAction::Restore {
        w.rail.settle(&attempt.unwrap(), true);
        drop(wallet);
        let failing = Arc::new(FailPauseStore::default());
        let base = Arc::clone(&failing) as Arc<dyn StateStore>;
        wallet = Box::pin(Paykit::restore(
            w.env(&w.alice, Arc::clone(&base)),
            paykit_sdk::PaykitConfig::default(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
            w.timer(),
        ))
        .await
        .unwrap();
        (failing, base)
    } else {
        (failing, base)
    };
    w.alice
        .switch()
        .make_divergent(UPLOADER, record, [1; 32])
        .unwrap();
    failing
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    if action == PauseAction::Repeat {
        w.clock.advance_ms(60_000);
    }
    let error = Box::pin(failed_pause_call(&mut wallet, &request, &record, action))
        .await
        .unwrap_err();
    assert!(
        failing.fired.load(std::sync::atomic::Ordering::SeqCst),
        "{error:?}"
    );
    assert!(format!("{error:?}").contains("Unavailable"), "{error:?}");
    other.receive(None).await.unwrap();
    let (allowance, _) = held(&other, &record).await;
    other
        .app_mut()
        .put_record(
            &allowances_target(&allowance),
            &allowance.to_json().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    for _ in 0..retries {
        failing
            .fired
            .store(false, std::sync::atomic::Ordering::SeqCst);
        failing
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = Box::pin(failed_pause_call(&mut wallet, &request, &record, action))
            .await
            .unwrap_err();
        assert!(
            failing.fired.load(std::sync::atomic::Ordering::SeqCst),
            "{error:?}"
        );
        assert!(format!("{error:?}").contains("Unavailable"), "{error:?}");
    }
    if reload {
        let id = wallet.app_id();
        drop(wallet);
        wallet = Box::pin(Paykit::load(
            w.env(&w.alice, base),
            paykit_sdk::PaykitConfig::default(),
            id,
            Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
            w.timer(),
        ))
        .await
        .unwrap();
    }
    match action {
        PauseAction::Direct | PauseAction::Grant => {
            if action == PauseAction::Grant {
                let refused = Box::pin(wallet.grant_executor(&record, UserConfirmed::new()))
                    .await
                    .unwrap_err();
                assert!(
                    matches!(refused.refusal(), Some(Refusal::NotNow(_))),
                    "{refused:?}"
                );
            }
            let second = Box::pin(wallet.pay_under_allowance(&request, &record)).await;
            assert!(
                second.as_ref().err().is_some_and(not_automatic),
                "paid after a failed pause commit: {second:?}"
            );
            assert!(w.rail.all_executions().is_empty());
        }
        PauseAction::Repeat => {
            Box::pin(wallet.resume_payments()).await.unwrap();
            assert_eq!(
                w.rail.pay_calls(&attempt.unwrap()),
                1,
                "repeated after a failed pause commit"
            );
        }
        PauseAction::Restore => {
            let second = Box::pin(wallet.reconcile()).await.unwrap();
            assert!(
                second.resolved.is_empty(),
                "resolved after a failed pause commit: {second:?}"
            );
            assert!(
                second
                    .doubtful
                    .iter()
                    .any(|d| d.attempts.contains(&attempt.unwrap())),
                "{second:?}"
            );
        }
    }
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    match action {
        PauseAction::Direct | PauseAction::Grant => {
            if action == PauseAction::Grant {
                Box::pin(wallet.grant_executor(&record, UserConfirmed::new()))
                    .await
                    .unwrap();
            }
            let confirmed = Box::pin(wallet.pay_under_allowance(&request, &record)).await;
            assert!(
                matches!(confirmed, Ok(PayOutcome::Paid { .. })),
                "{confirmed:?}"
            );
        }
        PauseAction::Repeat => {
            Box::pin(wallet.resume_payments()).await.unwrap();
            assert_eq!(w.rail.pay_calls(&attempt.unwrap()), 2);
        }
        PauseAction::Restore => {
            let confirmed = Box::pin(wallet.reconcile()).await.unwrap();
            assert!(
                matches!(confirmed.resolved.as_slice(), [PayOutcome::Paid { .. }]),
                "{confirmed:?}"
            );
            assert_eq!(w.rail.executions(&attempt.unwrap()), 1);
        }
    }
}

#[tokio::test]
async fn test_executor_direct_failed_pause_retry() {
    Box::pin(failed_pause_retry(PauseAction::Direct, false, 0)).await;
}
#[tokio::test]
async fn test_executor_grant_failed_pause_retry() {
    Box::pin(failed_pause_retry(PauseAction::Grant, false, 0)).await;
}
#[tokio::test]
async fn test_executor_repeat_failed_pause_retry() {
    Box::pin(failed_pause_retry(PauseAction::Repeat, false, 0)).await;
}
#[tokio::test]
async fn test_executor_restore_failed_pause_retry() {
    Box::pin(failed_pause_retry(PauseAction::Restore, false, 0)).await;
}
#[tokio::test]
async fn test_executor_direct_failed_pause_reload() {
    Box::pin(failed_pause_retry(PauseAction::Direct, true, 0)).await;
}
#[tokio::test]
async fn test_executor_grant_failed_pause_reload() {
    Box::pin(failed_pause_retry(PauseAction::Grant, true, 0)).await;
}
#[tokio::test]
async fn test_executor_repeat_failed_pause_reload() {
    Box::pin(failed_pause_retry(PauseAction::Repeat, true, 0)).await;
}
#[tokio::test]
async fn test_executor_restore_failed_pause_reload() {
    Box::pin(failed_pause_retry(PauseAction::Restore, true, 0)).await;
}

#[tokio::test]
async fn test_executor_direct_failed_pause_repeated_retry() {
    Box::pin(failed_pause_retry(PauseAction::Direct, false, 2)).await;
}

#[tokio::test]
async fn test_executor_grant_failed_pause_repeated_retry() {
    Box::pin(failed_pause_retry(PauseAction::Grant, false, 2)).await;
}

#[tokio::test]
async fn test_executor_repeat_failed_pause_repeated_retry() {
    Box::pin(failed_pause_retry(PauseAction::Repeat, false, 2)).await;
}

#[tokio::test]
async fn test_executor_restore_failed_pause_repeated_retry() {
    Box::pin(failed_pause_retry(PauseAction::Restore, false, 2)).await;
}

#[tokio::test]
async fn test_executor_repeat_divergence_at_final_read() {
    let w = Arc::new(World::new());
    w.rail.set_declaration(
        paykit_sdk::rail::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let reads = Arc::new(ReadHook::new(Arc::clone(&w.router)));
    let (mut shop, mut wallet, record) = Box::pin(set_up_through(
        &w,
        crate::common::store(),
        Arc::clone(&reads) as Arc<dyn Transport>,
    ))
    .await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } =
        Box::pin(wallet.pay_under_allowance(&request, &record))
            .await
            .unwrap()
    else {
        panic!("in flight")
    };
    let world = Arc::clone(&w);
    reads.after_read(
        &format!("/records/{}", pubky_switch_core::enc::b64(&record)),
        move || {
            world
                .alice
                .switch()
                .make_divergent(UPLOADER, record, [1; 32])
                .unwrap();
        },
    );
    w.clock.advance_ms(60_000);
    Box::pin(wallet.resume_payments()).await.unwrap();
    assert!(reads.fired());
    let shared = wallet.app().shared_record(&record).await.unwrap().unwrap();
    assert!(shared.divergent, "the final read must have seen divergence");
    assert_eq!(
        w.rail.pay_calls(&attempt_id),
        1,
        "repeated despite freshly observed divergence"
    );
}

#[tokio::test]
async fn test_executor_pending_divergence_preserves_other_duties_and_delivery() {
    use pubky_switch_client::app::AppEvent;

    let w = World::new();
    let (mut shop, mut wallet, record) = Box::pin(set_up(&w, terms_with(json!({})))).await;
    let request = Box::pin(request(&w, &mut shop, &mut wallet, "0.5")).await;
    // An unrelated request's refused effect is delivered to the host but remains owed.
    request_from(&mut shop, w.alice_id(), draft("0.3", w.clock.now_ms())).await;
    crate::common::refuse_writes(&w.alice, 1);
    wallet.app_mut().receive(None).await.unwrap();
    let before = wallet.app_mut().pending_duty_events().await.unwrap();
    let unrelated: Vec<_> = before
        .iter()
        .filter_map(|event| match event {
            AppEvent::EffectRefused { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert!(!unrelated.is_empty(), "{before:?}");
    w.alice
        .switch()
        .make_divergent(UPLOADER, record, [1; 32])
        .unwrap();
    let refused = Box::pin(wallet.pay_under_allowance(&request, &record)).await;
    assert!(
        refused.as_ref().err().is_some_and(not_automatic),
        "{refused:?}"
    );
    let after = wallet.app_mut().pending_duty_events().await.unwrap();
    for id in unrelated {
        assert!(
            after.iter().any(|event| event.id().as_ref() == Some(&id)),
            "{after:?}"
        );
    }
    let divergence = after
        .iter()
        .find_map(|event| match event {
            AppEvent::RecordDivergent { id, key } if key == &record => Some(*id),
            _ => None,
        })
        .expect("the divergence duty remains unacknowledged");
    let delivered = wallet.app_mut().take_events();
    assert!(
        delivered
            .iter()
            .any(|event| event.id().as_ref() == Some(&divergence)),
        "{delivered:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}
