//! The executor (Paykit spec 8.3): the grant, the ledger, and paying a one-time request
//! automatically under an Allowance.

use paykit_lib::allowances::{AllowanceTerms, Ledger};
use paykit_lib::records::AllowanceRecord;
use paykit_sdk::{
    AllowanceRole, AppScopes, PayOptions, PayOutcome, Paykit, PaykitError, Refusal, RequestRef,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use serde_json::{json, Value};

use crate::common::{draft, request_from, World};

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

/// Alice's executor wallet: `pay`, `allowance`, `allowance_execute` and `contacts`; she
/// admits bob.
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

/// Bob's shop, holding `request` and `allowance`; he admits alice.
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
async fn test_executor_grant_refused_without_allowance_execute() {
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
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "consent list"), "{refused:?}");
    // The user confirms the Allowance again, and the request held since the restore.
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    let refused = wallet
        .pay_under_allowance(&request, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "held"), "{refused:?}");
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    let paid = wallet.pay_under_allowance(&request, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
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
    // The new executor holds no authenticated ledger version as a reader, so it keeps none
    // at the handoff: it pays below as an executor that never held one.
    assert!(new.app().protocol_scan(b"lv/").await.unwrap().is_empty());
    let second = request(&w, &mut shop, &mut old, "0.5").await;
    let refused = old.pay_under_allowance(&second, &record).await.unwrap_err();
    assert!(
        refused_for(&refused, "another app is the Allowance's executor"),
        "{refused:?}"
    );
    // The notice takes the Allowance off the previous executor's consent list.
    old.receive(None).await.unwrap();
    assert!(!old.allowance(&record).await.unwrap().consented);
    // The new executor never held a ledger version: it builds on the previous executor's
    // ledger shown, once it passes every other check, and keeps it (Paykit spec 8.3, "Last
    // ledger version").
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
    // Another own app holding `allowance_execute`, never the executor, writes the ledger.
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
async fn restored_confirmed(
    w: &World,
    wallet: Paykit,
    record: &[u8; 32],
    request: Option<&RequestRef>,
) -> Paykit {
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
    if let Some(request) = request {
        wallet
            .confirm_possibly_cancelled(request, UserConfirmed::new())
            .await
            .unwrap();
    }
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
    let request = request(&w, &mut shop, &mut wallet, "0.5").await;
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
    let mut wallet = restored_confirmed(&w, wallet, &record, Some(&request)).await;
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
    let after = request(&w, &mut shop, &mut wallet, "0.3").await;
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
        .pay_under_allowance(&after, &record)
        .await
        .unwrap_err();
    assert!(refused_for(&refused, "consent list"), "{refused:?}");
    wallet
        .confirm_allowance_again(&record, UserConfirmed::new())
        .await
        .unwrap();
    wallet
        .confirm_possibly_cancelled(&after, UserConfirmed::new())
        .await
        .unwrap();
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
    w.rail.set_declaration(
        paykit_sdk::rail::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
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
    (shop, wallet, record, attempt_id)
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
async fn test_executor_no_repeat_under_an_allowance_ended_since() {
    // Bob ends the Allowance while the dispatch is in flight: the repeat's gate finds it
    // ended, and the method isn't called again (Paykit spec 7.2 step 1, 8.3).
    let w = World::new();
    let (mut shop, mut wallet, _, attempt) = executor_in_flight(&w).await;
    shop.receive(None).await.unwrap();
    let theirs = shop.allowances().await.unwrap()[0].record;
    shop.end_allowance(&theirs).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
}

#[tokio::test]
async fn test_executor_no_repeat_under_an_allowance_conflicted_since() {
    // Bob proposes again under the Allowance ID while the dispatch is in flight: the
    // Allowance is conflicted, and the method isn't called again (Paykit spec 8.3,
    // "Conflicted").
    let w = World::new();
    let (_shop, mut wallet, record, attempt) = executor_in_flight(&w).await;
    propose_again(&w, &wallet, &record).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
}

#[tokio::test]
async fn test_executor_no_repeat_while_the_allowance_needs_confirming_again() {
    // A `missed` item while the dispatch is in flight: every Allowance on the consent list
    // is to be confirmed again, so the method isn't called again until the user does
    // (Paykit spec 7.2, 8.3).
    let w = World::new();
    let (_shop, mut wallet, _, attempt) = executor_in_flight(&w).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(calls_after_resume(&w, &mut wallet, &attempt).await, 1);
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
    let third = request(&w, &mut shop, &mut wallet, "0.4").await;
    let fourth = request(&w, &mut shop, &mut wallet, "0.1").await;
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
    for held in [&third, &fourth] {
        wallet
            .confirm_possibly_cancelled(held, UserConfirmed::new())
            .await
            .unwrap();
    }
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
    // ledger, and money moves; the read that would keep that version fails. The switch then
    // answers `404` for the ledger: the executor pays nothing over the lifetime limit.
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
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: path.clone(),
            after: 1,
        });
    let paid = wallet.pay_under_allowance(&first, &record).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.record_writes(&w.alice, &key), 1);
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
    let refused = {
        let request = request(&w, &mut shop, &mut wallet, "0.5").await;
        let refused = wallet
            .pay_under_allowance(&request, &record)
            .await
            .unwrap_err();
        assert!(not_automatic(&refused), "{refused:?}");
        request
    };
    let mut wallet = restored_confirmed(&w, wallet, &record, Some(&refused)).await;
    let after = wallet.pay_under_allowance(&refused, &record).await;
    assert!(after.as_ref().is_err_and(not_automatic), "{after:?}");
    assert!(w.rail.all_executions().is_empty());
}
